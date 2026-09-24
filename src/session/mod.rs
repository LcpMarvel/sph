//! 统一浏览器会话：账号档案与视频号助手登录编排。
//!
//! 助手会话的本质是持久化 Chromium profile（~/.sph/accounts/<name>/profile/）：
//! 登录态留在 profile 里，后续命令直接复用，无需采集或存储任何 cookie。
//! 档案文件 account.json 只记元信息（名称、上次登录时间），不含秘密。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::{self, fetch, SharedWriter};
use crate::login::watch::{CookieEntry, LoginWatch, Poll};

pub const DEFAULT_ACCOUNT: &str = "default";
pub const ACCOUNT_FILE: &str = "account.json";
pub const PROFILE_DIR: &str = "profile";
pub const ASSISTANT_HOME: &str = "https://channels.weixin.qq.com/platform";
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 登录检测轮询的目标端点：助手平台域，登录成功后会出现新的会话 cookie。
const ASSISTANT_COOKIE_ENDPOINT: &str = "https://channels.weixin.qq.com/platform";

/// 账号档案（account.json 的格式）。不含任何秘密。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub version: i64,
    pub name: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub assistant_logged_in_at: Option<OffsetDateTime>,
}

impl Default for Account {
    fn default() -> Account {
        Account {
            version: 1,
            name: DEFAULT_ACCOUNT.into(),
            assistant_logged_in_at: None,
        }
    }
}

/// 账号在 ~/.sph/accounts/<name>/ 下的路径布局。
pub struct AccountDir {
    pub root: PathBuf,
}

impl AccountDir {
    pub fn new(accounts_root: &Path, name: &str) -> AccountDir {
        AccountDir {
            root: accounts_root.join(name),
        }
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.root.join(PROFILE_DIR)
    }

    pub fn account_file(&self) -> PathBuf {
        self.root.join(ACCOUNT_FILE)
    }

    pub fn profile_exists(&self) -> bool {
        self.profile_dir().is_dir()
    }

    pub fn load(&self) -> Result<Account> {
        let path = self.account_file();
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Account::default()),
            Err(e) => Err(AppError::fmt(
                Code::IOError,
                Stage::SessionLoad,
                format_args!("无法读取账号档案: {e}"),
            )),
            Ok(raw) => serde_json::from_str(&raw).map_err(|_| {
                AppError::new(
                    Code::InvalidCredentials,
                    Stage::SessionLoad,
                    "账号档案不是有效的 JSON",
                )
            }),
        }
    }

    pub fn save(&self, account: &Account) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::SessionLoad,
                format_args!("无法创建账号目录: {e}"),
            )
        })?;
        let raw = serde_json::to_string_pretty(account).map_err(|e| {
            AppError::fmt(
                Code::InternalError,
                Stage::SessionLoad,
                format_args!("档案序列化失败: {e}"),
            )
        })?;
        // 档案不含秘密，但沿用凭证目录的权限纪律
        atomic_write(&self.account_file(), raw.as_bytes())?;
        Ok(())
    }

    /// 删除账号的本地会话（profile）与档案。调用方负责确认。
    pub fn remove_session(&self) -> Result<()> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::SessionLoad,
                    format_args!("无法删除账号目录: {e}"),
                )
            })?;
        }
        Ok(())
    }
}

pub fn accounts_root(config_dir: &Path) -> PathBuf {
    config_dir.join("accounts")
}

/// 列出全部账号名（accounts/ 目录的一级子目录）。
pub fn list_accounts(config_dir: &Path) -> Vec<String> {
    let root = accounts_root(config_dir);
    let mut out: Vec<String> = std::fs::read_dir(&root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    use rand::Rng;
    let tmp = path.with_extension(format!("tmp-{:016x}", rand::thread_rng().gen::<u64>()));
    std::fs::write(&tmp, data).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::SessionLoad,
            format_args!("无法写入档案: {e}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::SessionLoad,
            format_args!("无法提交档案: {e}"),
        )
    })
}

/// 助手登录的可注入抽象（测试永不需要真实浏览器）。
#[async_trait::async_trait]
pub trait AssistantBrowser: Send {
    /// 采集适用于目标端点的 cookie（登录检测输入）。
    async fn cookies(&self, endpoint: &str) -> std::result::Result<Vec<CookieEntry>, String>;
    async fn close(&mut self);
}

pub struct LoginOptions {
    pub timeout: Duration,
    pub stderr: SharedWriter,
    /// 测试注入的假浏览器。
    pub launch: Option<std::sync::Arc<LaunchAssistantFn>>,
    pub now: Option<Arc<dyn Fn() -> OffsetDateTime + Send + Sync>>,
}

pub type LaunchAssistantFn = dyn Fn(&Path) -> futures::future::BoxFuture<'static, Result<Box<dyn AssistantBrowser>>>
    + Send
    + Sync;

impl Default for LoginOptions {
    fn default() -> LoginOptions {
        LoginOptions {
            timeout: Duration::from_secs(300),
            stderr: Arc::new(Mutex::new(std::io::stderr())),
            launch: None,
            now: None,
        }
    }
}

fn werr(stderr: &SharedWriter, args: std::fmt::Arguments) {
    if let Ok(mut g) = stderr.lock() {
        let _ = g.write_fmt(args);
        let _ = g.flush();
    }
}

/// 视频号助手扫码登录：打开可见浏览器（持久 profile），轮询 cookie 状态机
/// 检测登录，检测到后优雅关闭（profile 落盘）并写档案。
/// 任何失败路径都不破坏现有 profile。
pub async fn login_assistant(
    config_dir: &Path,
    account_name: &str,
    interactive: bool,
    opts: LoginOptions,
) -> Result<()> {
    if !interactive {
        return Err(AppError::new(
            Code::InteractiveRequired,
            Stage::LoginBrowser,
            "login 需要交互式终端；请在本机终端中运行 sph login",
        ));
    }
    let account_dir = AccountDir::new(&accounts_root(config_dir), account_name);
    let profile_dir = account_dir.profile_dir();
    std::fs::create_dir_all(&profile_dir).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::SessionLoad,
            format_args!("无法创建账号目录: {e}"),
        )
    })?;

    let stderr = opts.stderr.clone();
    werr(&stderr, format_args!("正在打开专用浏览器（视频号助手）…\n"));

    let now: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = opts
        .now
        .unwrap_or_else(|| Arc::new(OffsetDateTime::now_utc));

    let timeout = if opts.timeout.is_zero() {
        Duration::from_secs(300)
    } else {
        opts.timeout
    };

    let mut session: Box<dyn AssistantBrowser> = match &opts.launch {
        Some(launch) => launch(&profile_dir).await?,
        None => {
            let chrome = fetch::ensure_chromium(stderr.clone()).await?;
            Box::new(RealAssistantBrowser::open(&chrome, &profile_dir).await?)
        }
    };
    werr(
        &stderr,
        format_args!(
            "请在打开的浏览器窗口中扫码登录视频号助手。\n\
             登录后将自动继续并关闭浏览器；Ctrl+C 取消。\n"
        ),
    );

    let login_result = wait_login_detected(&mut *session, timeout).await;
    session.close().await;
    login_result?;
    // 让浏览器进程完全退出、profile 完全落盘
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut account = account_dir.load().unwrap_or_default();
    account.name = account_name.to_string();
    account.assistant_logged_in_at = Some(now());
    account_dir.save(&account)?;
    werr(
        &stderr,
        format_args!("视频号助手会话已保存（账号: {account_name}）。\n"),
    );
    Ok(())
}

/// 轮询目标端点 cookie jar，复用三段式登录状态机。
async fn wait_login_detected(session: &mut dyn AssistantBrowser, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut watch = LoginWatch::new();
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.tick().await;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::new(
                Code::Timeout,
                Stage::LoginBrowser,
                "登录超时（包含等待人工扫码的时间）",
            ));
        }
        tokio::select! {
            _ = tick.tick() => {
                match session.cookies(ASSISTANT_COOKIE_ENDPOINT).await {
                    Err(_) => continue,
                    Ok(mut cookies) => {
                        cookies.sort_by(|a, b| a.name.cmp(&b.name));
                        if watch.poll(&cookies) == Poll::Detected {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// 生产浏览器会话（chromiumoxide）。
pub struct RealAssistantBrowser {
    _browser: chromiumoxide::browser::Browser,
    page: chromiumoxide::Page,
}

impl RealAssistantBrowser {
    pub async fn open(chrome_path: &Path, profile_dir: &Path) -> Result<RealAssistantBrowser> {
        let (browser, page) = browser::launch(chrome_path, profile_dir, true).await?;
        page.goto(ASSISTANT_HOME).await.map_err(|e| {
            AppError::fmt(
                Code::LoginBrowserFailed,
                Stage::LoginBrowser,
                format_args!("无法打开视频号助手页面: {e}"),
            )
        })?;
        let _ = tokio::time::timeout(Duration::from_secs(30), page.wait_for_navigation()).await;
        Ok(RealAssistantBrowser {
            _browser: browser,
            page,
        })
    }
}

#[async_trait::async_trait]
impl AssistantBrowser for RealAssistantBrowser {
    async fn cookies(&self, endpoint: &str) -> std::result::Result<Vec<CookieEntry>, String> {
        use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams;
        let resp = self
            .page
            .execute(GetCookiesParams {
                urls: Some(vec![endpoint.to_string()]),
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(resp
            .result
            .cookies
            .into_iter()
            .map(|c| CookieEntry {
                name: c.name,
                value: c.value,
            })
            .collect())
    }

    async fn close(&mut self) {
        let _ = self._browser.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-session-test-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    struct FakeBrowser {
        script: StdMutex<VecDeque<Vec<CookieEntry>>>,
        poll: StdMutex<usize>,
        closed: StdMutex<bool>,
    }

    impl FakeBrowser {
        fn new(script: Vec<Vec<CookieEntry>>) -> FakeBrowser {
            FakeBrowser {
                script: StdMutex::new(script.into_iter().collect()),
                poll: StdMutex::new(0),
                closed: StdMutex::new(false),
            }
        }

        fn was_closed(&self) -> bool {
            *self.closed.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl AssistantBrowser for FakeBrowser {
        async fn cookies(&self, _endpoint: &str) -> std::result::Result<Vec<CookieEntry>, String> {
            let s = self.script.lock().unwrap();
            let mut p = self.poll.lock().unwrap();
            if s.is_empty() {
                return Ok(vec![]);
            }
            let idx = (*p).min(s.len() - 1);
            *p += 1;
            Ok(s.get(idx).cloned().unwrap_or_default())
        }

        async fn close(&mut self) {
            *self.closed.lock().unwrap() = true;
        }
    }

    struct ArcSession(Arc<FakeBrowser>);

    #[async_trait::async_trait]
    impl AssistantBrowser for ArcSession {
        async fn cookies(&self, endpoint: &str) -> std::result::Result<Vec<CookieEntry>, String> {
            self.0.cookies(endpoint).await
        }
        async fn close(&mut self) {
            *self.0.closed.lock().unwrap() = true;
        }
    }

    fn guest() -> Vec<CookieEntry> {
        vec![CookieEntry {
            name: "guest_a".into(),
            value: "1".into(),
        }]
    }

    fn logged_in() -> Vec<CookieEntry> {
        vec![
            CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            },
            CookieEntry {
                name: "sessionid".into(),
                value: "s".into(),
            },
        ]
    }

    #[tokio::test]
    async fn login_saves_account_and_keeps_profile_dir() {
        let base = tempdir();
        let config = base.join("cfg");
        let session = Arc::new(FakeBrowser::new(vec![
            guest(),
            guest(),
            guest(),
            logged_in(),
            logged_in(),
            logged_in(),
        ]));
        let session2 = session.clone();
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr: SharedWriter = stderr_vec.clone();
        let stderr2 = stderr_vec.clone();
        let opts = LoginOptions {
            timeout: Duration::from_secs(30),
            stderr,
            launch: Some(Arc::new(move |_profile| {
                let s = session2.clone();
                Box::pin(async move { Ok(Box::new(ArcSession(s)) as Box<dyn AssistantBrowser>) })
                    as futures::future::BoxFuture<'static, Result<Box<dyn AssistantBrowser>>>
            })),
            now: Some(Arc::new(|| {
                OffsetDateTime::from_unix_timestamp(1758187200).unwrap()
            })),
        };
        // profile 目录在登录前已被创建（浏览器要写盘）
        let profile = AccountDir::new(&accounts_root(&config), DEFAULT_ACCOUNT).profile_dir();
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Cookies"), b"fake-profile-data").unwrap();

        login_assistant(&config, DEFAULT_ACCOUNT, true, opts)
            .await
            .unwrap();
        assert!(session.was_closed(), "浏览器必须关闭");
        // profile 内容不被破坏
        assert_eq!(
            std::fs::read(profile.join("Cookies")).unwrap(),
            b"fake-profile-data"
        );
        let account = AccountDir::new(&accounts_root(&config), DEFAULT_ACCOUNT)
            .load()
            .unwrap();
        assert_eq!(account.name, "default");
        assert!(account.assistant_logged_in_at.is_some());
        let out = String::from_utf8_lossy(&stderr2.lock().unwrap()).into_owned();
        assert!(out.contains("视频号助手会话已保存"), "stderr: {out}");
    }

    #[tokio::test]
    async fn login_non_interactive() {
        let base = tempdir();
        let opts = LoginOptions::default();
        let err = login_assistant(&base.join("cfg"), DEFAULT_ACCOUNT, false, opts)
            .await
            .unwrap_err();
        assert_eq!(err.code, Code::InteractiveRequired);
    }

    #[tokio::test]
    async fn login_timeout_leaves_profile_intact() {
        let base = tempdir();
        let config = base.join("cfg");
        let session = Arc::new(FakeBrowser::new(vec![guest()]));
        let session2 = session.clone();
        let opts = LoginOptions {
            timeout: Duration::from_millis(250),
            stderr: Arc::new(Mutex::new(Vec::new())),
            launch: Some(Arc::new(move |_profile| {
                let s = session2.clone();
                Box::pin(async move { Ok(Box::new(ArcSession(s)) as Box<dyn AssistantBrowser>) })
                    as futures::future::BoxFuture<'static, Result<Box<dyn AssistantBrowser>>>
            })),
            now: None,
        };
        let profile = AccountDir::new(&accounts_root(&config), DEFAULT_ACCOUNT).profile_dir();
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Cookies"), b"old-data").unwrap();
        let err = login_assistant(&config, DEFAULT_ACCOUNT, true, opts)
            .await
            .unwrap_err();
        assert_eq!(err.code, Code::Timeout);
        assert!(session.was_closed());
        assert_eq!(
            std::fs::read(profile.join("Cookies")).unwrap(),
            b"old-data",
            "失败路径不得破坏 profile"
        );
        // 档案不应写入
        assert!(!AccountDir::new(&accounts_root(&config), DEFAULT_ACCOUNT)
            .account_file()
            .exists());
    }

    #[test]
    fn account_load_defaults_and_remove_session() {
        let base = tempdir();
        let dir = AccountDir::new(&accounts_root(&base), "default");
        // 无档案 → 默认
        let account = dir.load().unwrap();
        assert_eq!(account.name, "default");
        assert!(account.assistant_logged_in_at.is_none());
        // 保存/加载回环
        let mut account = account;
        account.assistant_logged_in_at =
            Some(OffsetDateTime::from_unix_timestamp(1758187200).unwrap());
        dir.save(&account).unwrap();
        let loaded = dir.load().unwrap();
        assert!(loaded.assistant_logged_in_at.is_some());
        // remove_session 删除整个账号目录
        std::fs::write(dir.profile_dir().join("Cookies"), b"x").ok();
        dir.remove_session().unwrap();
        assert!(!dir.root.exists());
        // 幂等
        dir.remove_session().unwrap();
    }

    #[test]
    fn list_accounts_sorted() {
        let base = tempdir();
        let root = accounts_root(&base);
        std::fs::create_dir_all(root.join("beta").join("profile")).unwrap();
        std::fs::create_dir_all(root.join("alpha").join("profile")).unwrap();
        std::fs::write(root.join("not-a-dir-file"), b"x").unwrap();
        let names = list_accounts(&base);
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }
}
