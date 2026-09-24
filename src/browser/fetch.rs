//! 浏览器可执行文件的获取：环境变量 → 系统浏览器 → 本机缓存 → 多镜像竞速下载。
//!
//! 优先使用系统 Chrome/Edge：免下载、无 "Chrome for Testing" 横幅，登录窗口观感正常。
//! 只有无系统浏览器时才退回 CfT 缓存/下载（headless 场景无观感问题）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::apperr::{AppError, Code, Result, Stage};

/// 固定的 chrome-for-testing 版本（与当前 CDP 领域兼容）。
const CFT_VERSION: &str = "131.0.6778.204";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

type SharedWriter = Arc<Mutex<dyn Write + Send>>;

struct Mirror {
    name: &'static str,
    url: String,
}

/// 查找系统已安装的 Chrome/Chromium/Edge。
pub fn system_chrome() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let candidates = [
            home.join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            home.join("Applications/Chromium.app/Contents/MacOS/Chromium"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
            home.join("Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            home.join("Applications/Arc.app/Contents/MacOS/Arc"),
        ];
        for c in candidates {
            if c.exists() {
                return Some(c);
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        for name in [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ] {
            if let Ok(out) = std::process::Command::new("which").arg(name).output() {
                if out.status.success() {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !path.is_empty() && Path::new(&path).exists() {
                        return Some(PathBuf::from(path));
                    }
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        let candidates = [
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
            PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
        ];
        for c in candidates {
            if c.exists() {
                return Some(c);
            }
        }
    }
    None
}

/// 确保本机有可用的浏览器可执行文件，返回其路径。
pub async fn ensure_chromium(stderr: SharedWriter) -> Result<PathBuf> {
    // 1) 环境变量直给
    if let Ok(p) = std::env::var("SPH_CHROME") {
        if !p.is_empty() && Path::new(&p).exists() {
            return Ok(PathBuf::from(p));
        }
    }
    // 2) 系统浏览器（首选：免下载、观感正常）
    if let Some(p) = system_chrome() {
        return Ok(p);
    }
    // 3) 本机 CfT 缓存
    let dir = browser_dir()?;
    let complete = dir.join(format!("chromium-{CFT_VERSION}.complete"));
    let binary = chromium_binary(&dir);
    if complete.exists() && binary.exists() {
        return Ok(binary);
    }

    // 4) 多镜像竞速下载
    if let Ok(mut g) = stderr.lock() {
        let _ = writeln!(
            g,
            "首次登录：正在下载专用浏览器（约 200MB，仅此一次，多镜像竞速）…"
        );
    }
    let platform = platform_id().ok_or_else(|| {
        AppError::new(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            "当前平台没有可用的专用浏览器下载源",
        )
    })?;
    let mirrors = vec![
        Mirror {
            name: "npmmirror",
            url: format!(
                "https://cdn.npmmirror.com/binaries/chrome-for-testing/{CFT_VERSION}/{platform}/chrome-{platform}.zip"
            ),
        },
        Mirror {
            name: "google",
            url: format!(
                "https://storage.googleapis.com/chrome-for-testing-public/{CFT_VERSION}/{platform}/chrome-{platform}.zip"
            ),
        },
    ];

    let dir_for_task = dir.clone();
    let dl = tokio::time::timeout(DOWNLOAD_TIMEOUT, race_download(mirrors, dir_for_task)).await;

    match dl {
        Err(_) => Err(AppError::new(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            "下载专用浏览器超时，请检查网络后重试",
        )),
        Ok(Ok(())) => Ok(chromium_binary(&dir)),
        Ok(Err(e)) => Err(e),
    }
}

async fn race_download(mirrors: Vec<Mirror>, dir: PathBuf) -> Result<()> {
    use futures::future::BoxFuture;
    let mut tasks: Vec<BoxFuture<'static, std::result::Result<PathBuf, (String, String)>>> =
        Vec::new();
    for m in mirrors {
        let dir = dir.clone();
        tasks.push(Box::pin(async move {
            download_one(&m, dir)
                .await
                .map_err(|e| (m.name.to_string(), e.message))
        }));
    }
    // join_all 收集全部结果：任一成功即可，全部失败才报错
    let results = futures::future::join_all(tasks).await;
    for r in results {
        if r.is_ok() {
            return Ok(());
        }
    }
    // 所有镜像都失败：给出汇总
    Err(AppError::new(
        Code::LoginDependencyMissing,
        Stage::LoginDeps,
        "所有镜像源下载专用浏览器均失败，请检查网络后重试",
    ))
}

async fn download_one(mirror: &Mirror, dir: PathBuf) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| {
            AppError::fmt(
                Code::LoginDependencyMissing,
                Stage::LoginDeps,
                format_args!("下载器初始化失败: {e}"),
            )
        })?;
    let resp = client
        .get(&mirror.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| {
            AppError::fmt(
                Code::LoginDependencyMissing,
                Stage::LoginDeps,
                format_args!("镜像 {} 下载失败: {e}", mirror.name),
            )
        })?;
    let bytes = resp.bytes().await.map_err(|e| {
        AppError::fmt(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            format_args!("镜像 {} 读取失败: {e}", mirror.name),
        )
    })?;
    // 解压到临时目录，成功后整体换位
    let tmp = dir.join(format!(".download-{}", mirror.name));
    if tmp.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    std::fs::create_dir_all(&tmp).map_err(|e| {
        AppError::fmt(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            format_args!("创建下载目录失败: {e}"),
        )
    })?;
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| {
        AppError::fmt(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            format_args!("浏览器压缩包损坏: {e}"),
        )
    })?;
    archive.extract(&tmp).map_err(|e| {
        AppError::fmt(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            format_args!("解压浏览器失败: {e}"),
        )
    })?;
    // 原子发布：rename 进缓存目录
    let platform_id = platform_id_static();
    let target = dir.join(format!("chrome-{platform_id}-{CFT_VERSION}"));
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        AppError::fmt(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            format_args!("发布浏览器目录失败: {e}"),
        )
    })?;
    // 可执行权限
    let binary = chromium_binary(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755));
    }
    // 完成标记
    std::fs::write(
        dir.join(format!("chromium-{CFT_VERSION}.complete")),
        b"ok\n",
    )
    .map_err(|e| {
        AppError::fmt(
            Code::LoginDependencyMissing,
            Stage::LoginDeps,
            format_args!("写入完成标记失败: {e}"),
        )
    })?;
    Ok(binary)
}

fn platform_id_static() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        ("linux", "x86_64") => "linux64",
        ("windows", "x86_64") => "win64",
        ("windows", "x86") => "win32",
        _ => "unknown",
    }
}

fn platform_id() -> Option<&'static str> {
    let id = platform_id_static();
    if id == "unknown" {
        None
    } else {
        Some(id)
    }
}

fn chromium_binary(dir: &Path) -> PathBuf {
    let base = dir.join(format!("chrome-{}-{CFT_VERSION}", platform_id_static()));
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => base.join(format!(
            "chrome-{}/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            platform_id_static()
        )),
        ("linux", _) => base.join(format!("chrome-{}/chrome", platform_id_static())),
        ("windows", _) => base.join(format!("chrome-{}/chrome.exe", platform_id_static())),
        _ => base,
    }
}

fn browser_dir() -> Result<PathBuf> {
    let dir = if let Ok(d) = std::env::var("SPH_CONFIG_DIR") {
        if !d.is_empty() {
            PathBuf::from(d)
        } else {
            default_browser_dir()
        }
    } else {
        default_browser_dir()
    }
    .join("browser");
    std::fs::create_dir_all(&dir).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::LoginDeps,
            format_args!("无法创建浏览器缓存目录: {e}"),
        )
    })?;
    Ok(dir)
}

fn default_browser_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".sph"))
        .unwrap_or_else(|| PathBuf::from(".sph"))
}
