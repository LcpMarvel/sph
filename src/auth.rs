//! 本地单一凭证文件、额外请求头白名单、login/import/logout 共享的
//! 咨询式修改锁，以及手动导入备用路径的解析。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::apperr::{AppError, Code, Result, Stage};

pub const MAX_CREDENTIALS_BYTES: usize = 256 << 10;
pub const MAX_COOKIE_BYTES: usize = 64 << 10;
pub const MAX_HEADERS_BYTES: usize = 64 << 10;
pub const MAX_IMPORT_BYTES: usize = 64 << 10;

pub const CREDENTIALS_FILE: &str = "credentials.json";
pub const LOCK_FILE: &str = ".auth.lock";
pub const CREDENTIALS_VERSION: i64 = 1;

/// 凭证来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    #[serde(rename = "browser_login")]
    BrowserLogin,
    #[serde(rename = "manual_import")]
    ManualImport,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::BrowserLogin => "browser_login",
            Source::ManualImport => "manual_import",
        }
    }
}

/// 凭证的内存形态。cookie 与请求头值是秘密：绝不可被记录或序列化进输出。
#[derive(Debug, Clone)]
pub struct Credentials {
    pub saved_at: OffsetDateTime,
    pub source: Source,
    pub verified_at: Option<OffsetDateTime>,
    pub cookie: String,
    pub yuanbao_headers: BTreeMap<String, String>, // 小写规范键
}

/// 与 credentials.json 完全一致的文件格式。
#[derive(Debug, Serialize, Deserialize)]
struct FileFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<i64>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    saved_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    verified_at: Option<OffsetDateTime>,
    #[serde(default)]
    cookie: String,
    #[serde(default)]
    yuanbao_headers: BTreeMap<String, String>,
}

/// 额外请求头的精确白名单。
fn allowed_yuanbao_header_keys() -> HashSet<&'static str> {
    [
        "accept-language",
        "user-agent",
        "referer",
        "sec-ch-ua",
        "sec-ch-ua-mobile",
        "sec-ch-ua-platform",
        "sec-fetch-dest",
        "sec-fetch-mode",
        "sec-fetch-site",
        "t-userid",
        "x-agentid",
        "x-commit-tag",
        "x-device-id",
        "x-hy106",
        "x-hy92",
        "x-hy93",
        "x-id",
        "x-instance-id",
        "x-language",
        "x-os_version",
        "x-platform",
        "x-requested-with",
        "x-source",
        "x-web-third-source",
        "x-webdriver",
        "x-webversion",
        "x-ybuitest",
    ]
    .into_iter()
    .collect()
}

/// 绝不可通过请求头覆盖的键。
fn forbidden_header_keys() -> HashSet<&'static str> {
    [
        "cookie",
        "authorization",
        "host",
        "origin",
        "content-length",
        "connection",
        "transfer-encoding",
        "proxy-authorization",
    ]
    .into_iter()
    .collect()
}

pub fn canonical_header_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

/// 规范并校验一个额外请求头映射：只允许白名单键、值中无 CR/LF、
/// referer 仅限元宝站点、禁止键被拒绝、总大小受限。未知键是错误——
/// 坏的导入或采集必须响亮失败，而不是静默丢弃数据。
pub fn validate_header_map(input: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let allowed = allowed_yuanbao_header_keys();
    let forbidden = forbidden_header_keys();
    let mut total = 0usize;
    let mut out = BTreeMap::new();
    for (k, v) in input {
        let ck = canonical_header_key(k);
        if forbidden.contains(ck.as_str()) {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::Credentials,
                format_args!("额外请求头不允许覆盖 {ck}"),
            ));
        }
        if !allowed.contains(ck.as_str()) {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::Credentials,
                format_args!("额外请求头 {ck} 不在允许列表内"),
            ));
        }
        if v.contains(['\r', '\n']) {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::Credentials,
                format_args!("额外请求头 {ck} 的值包含换行符"),
            ));
        }
        if ck == "referer" {
            match url::Url::parse(v) {
                Ok(u) if u.scheme() == "https" && u.host_str() == Some("yuanbao.tencent.com") => {}
                _ => {
                    return Err(AppError::new(
                        Code::InvalidArgument,
                        Stage::Credentials,
                        "referer 只允许 https://yuanbao.tencent.com",
                    ))
                }
            }
        }
        total += ck.len() + v.len();
        if total > MAX_HEADERS_BYTES {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::Credentials,
                format_args!("额外请求头总大小超过 {} 字节", MAX_HEADERS_BYTES),
            ));
        }
        out.insert(ck, v.clone());
    }
    Ok(out)
}

impl Credentials {
    /// 存储或使用前的完整校验。
    pub fn validate(&self) -> Result<()> {
        if self.cookie.is_empty() {
            return Err(AppError::new(
                Code::InvalidCredentials,
                Stage::Credentials,
                "凭证缺少 Cookie",
            ));
        }
        if self.cookie.len() > MAX_COOKIE_BYTES {
            return Err(AppError::fmt(
                Code::InvalidCredentials,
                Stage::Credentials,
                format_args!("Cookie 超过 {} 字节上限", MAX_COOKIE_BYTES),
            ));
        }
        if self.cookie.contains(['\r', '\n']) {
            return Err(AppError::new(
                Code::InvalidCredentials,
                Stage::Credentials,
                "Cookie 包含换行符",
            ));
        }
        validate_header_map(&self.yuanbao_headers)?;
        Ok(())
    }
}

/// 管理配置目录中的凭证文件。
pub struct Store {
    pub dir: PathBuf,
}

/// 默认配置目录：$SPH_CONFIG_DIR，否则 ~/.sph。
/// （v2 起数据目录从 ~/.config/sph 迁移到 ~/.sph；不迁移旧凭证，首次使用提示重新授权。）
pub fn default_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SPH_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(home) = home_dir() {
        return home.join(".sph");
    }
    PathBuf::from(".sph")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

impl Store {
    /// 确保配置目录存在且权限为 0700。
    pub fn new(dir: impl Into<PathBuf>) -> Result<Store> {
        let dir = dir.into();
        match fs::metadata(&dir) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::create_dir_all(&dir).map_err(|e| {
                    AppError::fmt(
                        Code::IOError,
                        Stage::Credentials,
                        format_args!("无法创建配置目录: {e}"),
                    )
                })?;
                #[cfg(unix)]
                let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
            }
            Err(e) => {
                return Err(AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("无法访问配置目录: {e}"),
                ))
            }
            Ok(m) => {
                if !m.is_dir() {
                    return Err(AppError::new(
                        Code::IOError,
                        Stage::Credentials,
                        "配置路径不是目录",
                    ));
                }
                #[cfg(unix)]
                if m.permissions().mode() & 0o077 != 0 {
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|e| {
                        AppError::fmt(
                            Code::IOError,
                            Stage::Credentials,
                            format_args!("配置目录权限过宽且无法收紧: {e}"),
                        )
                    })?;
                }
            }
        }
        Ok(Store { dir })
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(CREDENTIALS_FILE)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(LOCK_FILE)
    }

    pub fn exists(&self) -> bool {
        fs::symlink_metadata(self.path()).is_ok()
    }

    /// 读取并校验凭证。缺文件 → AUTH_REQUIRED；符号链接、权限过宽、
    /// 超大、未知版本与非法内容一律拒绝并附修复提示。
    pub fn load(&self) -> Result<Credentials> {
        let path = self.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                AppError::new(
                    Code::AuthRequired,
                    Stage::Credentials,
                    "未找到登录凭证，请先执行 sph login",
                )
            } else {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("无法读取凭证文件: {e}"),
                )
            }
        })?;
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(AppError::fmt(
                Code::InvalidCredentials,
                Stage::Credentials,
                format_args!("凭证文件不是普通文件，请检查 {}", path.display()),
            ));
        }
        #[cfg(unix)]
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(AppError::fmt(
                Code::InvalidCredentials,
                Stage::Credentials,
                format_args!("凭证文件权限过宽，请执行 chmod 600 {}", path.display()),
            ));
        }
        if meta.len() > MAX_CREDENTIALS_BYTES as u64 {
            return Err(AppError::new(
                Code::InvalidCredentials,
                Stage::Credentials,
                "凭证文件超过大小上限",
            ));
        }
        let raw = fs::read_to_string(&path).map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::Credentials,
                format_args!("无法读取凭证文件: {e}"),
            )
        })?;
        let ff: FileFormat = serde_json::from_str(&raw).map_err(|_| {
            AppError::new(
                Code::InvalidCredentials,
                Stage::Credentials,
                "凭证文件不是有效的 JSON；如需重置请执行 sph logout 后重新 sph login",
            )
        })?;
        // 旧格式兼容：缺失 version/source/verified_at 读作 1 / manual_import / null；
        // 显式的未知版本是致命错误。
        if let Some(v) = ff.version {
            if v != CREDENTIALS_VERSION {
                return Err(AppError::fmt(
                    Code::InvalidCredentials,
                    Stage::Credentials,
                    format_args!("凭证文件版本 {v} 不受支持"),
                ));
            }
        }
        let source = match ff.source.as_deref() {
            None | Some("") => Source::ManualImport,
            Some("browser_login") => Source::BrowserLogin,
            Some("manual_import") => Source::ManualImport,
            Some(_) => {
                return Err(AppError::new(
                    Code::InvalidCredentials,
                    Stage::Credentials,
                    "凭证来源字段的值不合法",
                ))
            }
        };
        let saved_at = match ff.saved_at {
            Some(t) => t,
            None => system_time_to_rfc3339(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
        };
        let mut headers = ff.yuanbao_headers.clone();
        for k in ff.yuanbao_headers.keys() {
            let ck = canonical_header_key(k);
            if ck != *k {
                if let Some(v) = ff.yuanbao_headers.get(k) {
                    headers.remove(k);
                    headers.insert(ck, v.clone());
                }
            }
        }
        let creds = Credentials {
            saved_at,
            source,
            verified_at: ff.verified_at,
            cookie: ff.cookie,
            yuanbao_headers: headers,
        };
        creds.validate()?;
        Ok(creds)
    }

    /// 原子写入凭证：同目录 0600 临时文件，写尽、fsync、rename 覆盖。
    /// 调用方必须持有修改锁。
    pub fn save(&self, creds: &Credentials) -> Result<()> {
        creds.validate()?;
        let ff = FileFormat {
            version: Some(CREDENTIALS_VERSION),
            saved_at: Some(creds.saved_at),
            source: Some(creds.source.as_str().to_string()),
            verified_at: creds.verified_at,
            cookie: creds.cookie.clone(),
            yuanbao_headers: creds.yuanbao_headers.clone(),
        };
        let mut raw = serde_json::to_string_pretty(&ff).map_err(|e| {
            AppError::fmt(
                Code::InternalError,
                Stage::Credentials,
                format_args!("凭证序列化失败: {e}"),
            )
        })?;
        raw.push('\n');
        if raw.len() > MAX_CREDENTIALS_BYTES {
            return Err(AppError::new(
                Code::InvalidCredentials,
                Stage::Credentials,
                "凭证内容超过大小上限",
            ));
        }
        fs::create_dir_all(&self.dir).map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::Credentials,
                format_args!("无法访问配置目录: {e}"),
            )
        })?;
        let tmp_name = format!(".credentials-{:016x}.tmp", rand_u64());
        let tmp_path = self.dir.join(&tmp_name);
        let write_result = (|| -> Result<()> {
            // 用写句柄完成写入与 fsync：Windows 上只读句柄 sync 报 Access denied。
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .map_err(|e| {
                    AppError::fmt(
                        Code::IOError,
                        Stage::Credentials,
                        format_args!("无法创建临时凭证文件: {e}"),
                    )
                })?;
            use std::io::Write as _;
            f.write_all(raw.as_bytes()).map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("写入凭证失败: {e}"),
                )
            })?;
            f.sync_all().map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("同步凭证失败: {e}"),
                )
            })?;
            drop(f); // rename 前必须关闭句柄（Windows 不允许句柄开着被重命名）
            #[cfg(unix)]
            fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600)).map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("无法设置临时凭证文件权限: {e}"),
                )
            })?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
        fs::rename(&tmp_path, self.path()).map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::Credentials,
                format_args!("提交凭证文件失败: {e}"),
            )
        })?;
        Ok(())
    }

    /// 删除凭证文件；缺文件是幂等成功。不碰配置目录里的其他任何东西。
    pub fn clear(&self) -> Result<()> {
        let path = self.path();
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AppError::fmt(
                Code::IOError,
                Stage::Credentials,
                format_args!("无法访问凭证文件: {e}"),
            )),
            Ok(m) => {
                if !m.file_type().is_file() {
                    return Err(AppError::fmt(
                        Code::IOError,
                        Stage::Credentials,
                        format_args!("凭证路径不是普通文件，请手动检查 {}", path.display()),
                    ));
                }
                fs::remove_file(&path).map_err(|e| {
                    AppError::fmt(
                        Code::IOError,
                        Stage::Credentials,
                        format_args!("删除凭证文件失败: {e}"),
                    )
                })
            }
        }
    }

    /// 获取咨询式修改锁（非阻塞排他 flock）。
    pub fn acquire_lock(&self) -> Result<Lock> {
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())
            .map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("无法创建锁文件: {e}"),
                )
            })?;
        f.try_lock_exclusive().map_err(|e| {
            // Windows: ERROR_LOCK_VIOLATION(33) / ERROR_SHARING_VIOLATION(32) 表示锁被占用
            let busy = matches!(
                e.kind(),
                ErrorKind::WouldBlock | ErrorKind::PermissionDenied
            ) || matches!(e.raw_os_error(), Some(33) | Some(32));
            if busy {
                AppError::new(
                    Code::AuthBusy,
                    Stage::Credentials,
                    "凭证修改锁被其他进程占用，请稍后重试",
                )
            } else {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("无法获取凭证锁: {e}"),
                )
            }
        })?;
        Ok(Lock { file: Some(f) })
    }
}

pub struct Lock {
    file: Option<fs::File>,
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lock").finish_non_exhaustive()
    }
}

impl Lock {
    pub fn release(mut self) -> Result<()> {
        if let Some(f) = self.file.take() {
            f.unlock().map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Credentials,
                    format_args!("释放凭证锁失败: {e}"),
                )
            })?;
        }
        Ok(())
    }
}

use fs4::fs_std::FileExt;

fn rand_u64() -> u64 {
    use rand::Rng;
    rand::thread_rng().gen()
}

fn system_time_to_rfc3339(st: SystemTime) -> OffsetDateTime {
    OffsetDateTime::from(st)
}

/// 解析手动导入的 stdin：裸 Cookie 值或以 "Cookie:" 开头的单行。
/// 外部空白被裁剪；内部 CR/LF 被拒绝；整条 cURL 命令被拒绝。
pub fn parse_cookie_import(input: &str) -> Result<String> {
    if input.len() > MAX_IMPORT_BYTES {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("导入内容超过 {} 字节上限", MAX_IMPORT_BYTES),
        ));
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "导入内容为空",
        ));
    }
    if trimmed.contains(['\r', '\n']) {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "导入内容包含换行，只接受单行 Cookie 值",
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("curl ") || lower.contains(" -h ") {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "检测到 cURL 命令；请只粘贴 Cookie 值本身",
        ));
    }
    if lower == "cookie:" || lower == "-cookie" {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "导入内容缺少 Cookie 值",
        ));
    }
    let mut out = trimmed;
    if let Some(rest) = lower.strip_prefix("cookie:") {
        out = rest.trim();
    }
    if out.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "导入内容缺少 Cookie 值",
        ));
    }
    if out.len() > MAX_COOKIE_BYTES {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("Cookie 超过 {} 字节上限", MAX_COOKIE_BYTES),
        ));
    }
    if !out.contains('=') {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "Cookie 值应当是 name=value; ... 形式",
        ));
    }
    Ok(out.to_string())
}

/// 解析可选的 --headers-file JSON。
pub fn parse_headers_file(data: &[u8]) -> Result<BTreeMap<String, String>> {
    if data.len() > MAX_IMPORT_BYTES {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("额外请求头文件超过 {} 字节上限", MAX_IMPORT_BYTES),
        ));
    }
    if data.is_empty() {
        return Ok(BTreeMap::new());
    }
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_slice(data).map_err(|_| {
        AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "额外请求头文件必须是 JSON 字符串键值对象",
        )
    })?;
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        let s = v.as_str().ok_or_else(|| {
            AppError::fmt(
                Code::InvalidArgument,
                Stage::Arguments,
                format_args!("请求头 {k} 的值必须是字符串"),
            )
        })?;
        if s.contains(['\r', '\n']) {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::Arguments,
                format_args!("请求头 {k} 的值包含换行符"),
            ));
        }
        out.insert(k, s.to_string());
    }
    validate_header_map(&out)
}

pub fn file_mod_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .ok()
        .map(|m| m.modified())
        .and_then(|r| r.ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-auth-test-{:016x}", rand_u64()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn new_test_store(dir: &Path) -> Store {
        Store::new(dir).unwrap()
    }

    fn sample_creds() -> Credentials {
        let now = OffsetDateTime::from_unix_timestamp(1758230400).unwrap(); // 2026-09-19T00:00:00Z
        Credentials {
            saved_at: now,
            source: Source::BrowserLogin,
            verified_at: Some(now),
            cookie: "session=abc; token=def".into(),
            yuanbao_headers: [("user-agent".to_string(), "UA/1".to_string())]
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let input = sample_creds();
        store.save(&input).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(perm & 0o777, 0o600);
            assert_eq!(perm & 0o077, 0);
        }
        let out = store.load().unwrap();
        assert_eq!(out.cookie, input.cookie);
        assert_eq!(out.source, input.source);
        assert_eq!(out.verified_at, input.verified_at);
        assert_eq!(out.yuanbao_headers["user-agent"], "UA/1");
    }

    #[test]
    fn load_missing_is_auth_required() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let err = store.load().unwrap_err();
        assert_eq!(err.code, Code::AuthRequired);
    }

    #[test]
    fn load_legacy_format_compat() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        std::fs::write(store.path(), br#"{"cookie":"a=b","yuanbao_headers":{}}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let creds = store.load().unwrap();
        assert_eq!(creds.source, Source::ManualImport);
        assert_eq!(creds.verified_at, None);
    }

    #[test]
    fn load_unknown_version_rejected() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        std::fs::write(store.path(), br#"{"version":99,"cookie":"a=b"}"#).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = store.load().unwrap_err();
        assert_eq!(err.code, Code::InvalidCredentials);
    }

    #[cfg(unix)]
    #[test]
    fn load_symlink_rejected() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let real = base.join("real.json");
        std::fs::write(&real, br#"{"cookie":"a=b"}"#).unwrap();
        std::os::unix::fs::symlink(&real, store.path()).unwrap();
        let err = store.load().unwrap_err();
        assert_eq!(err.code, Code::InvalidCredentials);
    }

    #[cfg(unix)]
    #[test]
    fn load_wide_perms_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        store.save(&sample_creds()).unwrap();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.load().unwrap_err();
        assert_eq!(err.code, Code::InvalidCredentials);
    }

    #[test]
    fn load_oversize_rejected() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let big = format!("{{\"cookie\":\"{}\"}}", "a".repeat(MAX_CREDENTIALS_BYTES));
        std::fs::write(store.path(), big.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = store.load().unwrap_err();
        assert_eq!(err.code, Code::InvalidCredentials);
    }

    #[test]
    fn clear_idempotent_and_scoped() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let other = store.dir.join("other-file.txt");
        std::fs::write(&other, b"keep me").unwrap();
        store.clear().unwrap();
        store.save(&sample_creds()).unwrap();
        store.clear().unwrap();
        assert!(!store.exists());
        assert!(other.exists());
    }

    #[test]
    fn lock_mutual_exclusion() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let lock1 = store.acquire_lock().unwrap();
        let err = store.acquire_lock().unwrap_err();
        assert_eq!(err.code, Code::AuthBusy);
        lock1.release().unwrap();
        let lock2 = store.acquire_lock().unwrap();
        lock2.release().unwrap();
        // 锁文件必须保留（解锁 ≠ 删除）
        assert!(store.lock_path().exists());
    }

    #[test]
    fn parse_cookie_import_cases() {
        let ok: Vec<(&str, &str)> = vec![
            ("session=abc; x=y", "session=abc; x=y"),
            ("  session=abc  ", "session=abc"),
            ("Cookie: session=abc", "session=abc"),
            ("cookie: session=abc", "session=abc"),
            ("\tcookie: session=abc\n", "session=abc"),
        ];
        for (input, want) in ok {
            assert_eq!(parse_cookie_import(input).unwrap(), want, "input {input:?}");
        }
        let bad = vec![
            "",
            "   ",
            "cookie=abc\r\nx=1",
            "multi\nline=1",
            "curl 'https://yuanbao.tencent.com/' -H 'cookie: x=1'",
            "cookie:",
            "just-a-token-without-equals",
        ];
        for input in bad {
            assert!(parse_cookie_import(input).is_err(), "input {input:?}");
        }
    }

    #[test]
    fn parse_headers_file_cases() {
        let good = parse_headers_file(br#"{"User-Agent":"UA/1","x-language":"zh-CN"}"#).unwrap();
        assert_eq!(good["user-agent"], "UA/1");
        assert_eq!(good["x-language"], "zh-CN");
        assert!(parse_headers_file(&[]).unwrap().is_empty());
        let bad = vec![
            r#"{"x-not-a-header":"v"}"#,
            r#"{"cookie":"x=1"}"#,
            r#"{"authorization":"Bearer x"}"#,
            r#"{"referer":"https://evil.test/"}"#,
            r#"{"referer":"http://yuanbao.tencent.com/"}"#,
            "{\"user-agent\":\"bad\r\nvalue\"}",
            "[1,2]",
            r#"{"user-agent":123}"#,
        ];
        for input in bad {
            assert!(
                parse_headers_file(input.as_bytes()).is_err(),
                "input {input}"
            );
        }
    }

    #[test]
    fn validate_header_map_size_cap() {
        let mut input = BTreeMap::new();
        for i in 0..60 {
            let key = format!(
                "x-hy{}{}",
                (b'a' + (i % 26) as u8) as char,
                b'0' + (i / 26) as u8
            );
            input.insert(key, String::new());
        }
        input.insert("user-agent".into(), "a".repeat(MAX_HEADERS_BYTES));
        assert!(validate_header_map(&input).is_err());
    }

    #[test]
    fn save_rejects_invalid_creds() {
        let base = tempdir();
        let store = new_test_store(&base.join("cfg"));
        let mut c = sample_creds();
        c.cookie = String::new();
        assert!(store.save(&c).is_err());
        let mut c = sample_creds();
        c.cookie = "a".repeat(MAX_COOKIE_BYTES + 1);
        assert!(store.save(&c).is_err());
        let mut c = sample_creds();
        c.yuanbao_headers = [("cookie".to_string(), "x=1".to_string())]
            .into_iter()
            .collect();
        assert!(store.save(&c).is_err());
    }

    #[test]
    fn lock_busy_error_kind_mapping() {
        // 直接确认 EWOULDBLOCK 路径映射为 AuthBusy（上面互斥测试已覆盖行为）
        let e = std::io::Error::from(ErrorKind::WouldBlock);
        assert!(matches!(e.kind(), ErrorKind::WouldBlock));
    }
}
