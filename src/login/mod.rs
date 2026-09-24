//! `sph login` 流程编排：专用可见 Chromium（一次性 profile）、用户亲自登录、
//! 端点级 cookie 采集、浏览器关闭后才原子提交。任何失败路径都不触碰现有凭证。
//!
//! 登录检测全自动：每秒轮询目标端点的 cookie jar 并过 loginWatch 状态机
//! （访客基线稳定 → 会话 cookie 名增长 → 指纹稳定 3 周期）。
//! 终端里手动按 Enter 是自动检测失效时的兜底。

pub mod browser;
pub mod watch;

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::auth::{self, Credentials, Source};
use watch::{CookieEntry, LoginWatch, Poll};

/// 整个登录流程的预算。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONSECUTIVE_ERROR_LIMIT: usize = 3;
/// 唯一的采集目标：只有适用于这个确切端点的 cookie 会被读取或存储。
pub const TARGET_ENDPOINT: &str = "https://yuanbao.tencent.com/api/weixin/get_parse_result";

type SharedWriter = Arc<Mutex<dyn Write + Send>>;

fn werr(stderr: &SharedWriter, args: std::fmt::Arguments) {
    if let Ok(mut g) = stderr.lock() {
        let _ = g.write_fmt(args);
        let _ = g.flush();
    }
}

/// 浏览器启动器类型别名（测试注入假实现）。
pub type LaunchFn = Arc<
    dyn Fn(
            &str,
            SharedWriter,
        ) -> futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        + Send
        + Sync,
>;

/// 专用登录浏览器的抽象，测试永远不需要启动真实浏览器。
#[async_trait::async_trait]
pub trait BrowserSession: Send {
    /// 返回适用于目标 URL 的 cookie（含 HttpOnly），与浏览器发往该端点时一致。
    async fn cookies(&self, target_url: &str) -> std::result::Result<Vec<CookieEntry>, String>;
    /// 浏览器自己的 navigator.userAgent（非敏感）。
    async fn user_agent(&self) -> std::result::Result<String, String>;
    async fn close(&mut self);
}

pub struct Options {
    pub timeout: Duration,
    pub stderr: SharedWriter,
    /// 测试注入的假浏览器启动器；None 表示生产实现。
    pub launch: Option<LaunchFn>,
    pub now: Option<Arc<dyn Fn() -> OffsetDateTime + Send + Sync>>,
    /// 测试注入的终端行源；None 表示读真实 stdin。
    pub stdin: Option<Box<dyn LineSource>>,
    /// 外部取消（Ctrl+C）。
    pub cancel: Option<crate::http::CancelToken>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            timeout: DEFAULT_TIMEOUT,
            stderr: Arc::new(Mutex::new(std::io::stderr())),
            launch: None,
            now: None,
            stdin: None,
            cancel: None,
        }
    }
}

/// stdin 行读取抽象（测试注入脚本化的行）。
pub trait LineSource: Send {
    /// 阻塞读取一行（不含换行符）；EOF 或失败返回 None。
    fn read_line(&mut self) -> Option<String>;
}

struct StdinLines;

impl LineSource for StdinLines {
    fn read_line(&mut self) -> Option<String> {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut line = String::new();
        match lock.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
            Err(_) => None,
        }
    }
}

/// 执行完整登录。只返回 apperr 错误；任何失败路径上现有凭证都不受影响。
pub async fn run(store: &auth::Store, interactive: bool, opts: Options) -> Result<()> {
    let stderr = opts.stderr.clone();
    if !interactive {
        return Err(AppError::new(
            Code::InteractiveRequired,
            Stage::LoginBrowser,
            "login 需要交互式终端；请在本机终端中运行 sph login",
        ));
    }

    let timeout = if opts.timeout.is_zero() {
        DEFAULT_TIMEOUT
    } else {
        opts.timeout
    };
    let now: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = opts
        .now
        .unwrap_or_else(|| Arc::new(OffsetDateTime::now_utc));

    // 同一时刻只允许一次修改
    let lock = store.acquire_lock()?;

    let work_dir = make_work_dir(&store.dir)?;
    let work_dir_cleanup = work_dir.clone();
    let stderr_cleanup = stderr.clone();
    let cleanup = scopeguard::guard((), move |_| {
        if std::fs::remove_dir_all(&work_dir_cleanup).is_err() {
            werr(
                &stderr_cleanup,
                format_args!(
                    "警告：清理登录临时目录失败，请手动删除 {}\n",
                    work_dir_cleanup.display()
                ),
            );
        }
    });

    let mut session: Box<dyn BrowserSession> = match &opts.launch {
        Some(launch) => {
            werr(&stderr, format_args!("正在打开专用浏览器（元宝官网）…\n"));
            launch(work_dir_str(&work_dir), stderr.clone()).await?
        }
        None => {
            werr(&stderr, format_args!("正在打开专用浏览器（元宝官网）…\n"));
            Box::new(browser::launch(work_dir_str(&work_dir), stderr.clone()).await?)
        }
    };

    werr(
        &stderr,
        format_args!(
            "请在打开的浏览器窗口中登录 yuanbao.tencent.com（扫码或账号认证）。\n\
             登录后将自动继续并关闭浏览器；若长时间未自动继续，可回此终端按 Enter 手动继续；Ctrl+C 取消。\n"
        ),
    );

    let cookies =
        match wait_for_login(timeout, session.as_mut(), opts.stdin, opts.cancel.clone()).await {
            Ok(c) => c,
            Err(e) => {
                session.close().await;
                return Err(e);
            }
        };

    let creds = match build_credentials(session.as_mut(), &cookies).await {
        Ok(c) => c,
        Err(e) => {
            session.close().await;
            return Err(e);
        }
    };

    // 采集通过：先关浏览器，再提交。
    session.close().await;
    tokio::time::sleep(Duration::from_millis(200)).await; // 等浏览器进程完全退出后再删除 profile

    let creds = Credentials {
        saved_at: now(),
        source: Source::BrowserLogin,
        verified_at: None,
        ..creds
    };
    store.save(&creds).map_err(|e| AppError {
        code: Code::IOError,
        stage: Stage::LoginCommit,
        ..e
    })?;
    werr(&stderr, format_args!("登录凭证已保存，浏览器已关闭。\n"));
    drop(lock);
    drop(cleanup);
    Ok(())
}

/// 轮询目标端点的 cookie jar，直到状态机判定登录。终端 Enter 强制提前采集。
/// 连续的 cookie 错误意味着用户关闭了浏览器窗口。
async fn wait_for_login(
    timeout: Duration,
    session: &mut dyn BrowserSession,
    stdin: Option<Box<dyn LineSource>>,
    cancel: Option<crate::http::CancelToken>,
) -> Result<Vec<CookieEntry>> {
    let deadline = tokio::time::Instant::now() + timeout;

    // 终端 Enter 监听（单独线程阻塞读行源；真实 stdin 或测试注入）
    let (manual_tx, mut manual_rx) = tokio::sync::mpsc::channel::<()>(1);
    std::thread::spawn(move || {
        let mut source: Box<dyn LineSource> = stdin.unwrap_or_else(|| Box::new(StdinLines));
        // 只读一行：EOF 直接退出（不触发手动继续），读到内容则通知一次
        if source.read_line().is_some() {
            let _ = manual_tx.blocking_send(());
        }
    });

    let mut watch = LoginWatch::new();
    let mut consecutive_errors = 0usize;
    let mut manual_open = true; // 行源 EOF 后置 false：不再期待手动触发
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.tick().await; // 立即的第一跳
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::new(
                Code::Timeout,
                Stage::LoginBrowser,
                "登录超时（包含等待人工登录与采集的时间）",
            ));
        }
        let cancel_fut = async {
            match &cancel {
                Some(c) => {
                    while !c.cancelled() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
                None => futures::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancel_fut);
        let poll_once = async {
            match session.cookies(TARGET_ENDPOINT).await {
                Err(_) => Err(()),
                Ok(mut cookies) => {
                    consecutive_errors = 0;
                    sort_cookies(&mut cookies);
                    Ok(cookies)
                }
            }
        };
        if manual_open {
            tokio::select! {
                _ = &mut cancel_fut => {
                    return Err(AppError::new(Code::Cancelled, Stage::LoginBrowser, "登录已取消"));
                }
                v = manual_rx.recv() => {
                    match v {
                        Some(()) => {
                            return session.cookies(TARGET_ENDPOINT).await.map_err(|_| {
                                AppError::new(Code::LoginBrowserFailed, Stage::LoginBrowser, "浏览器已不可用")
                            }).map(|mut cs| { sort_cookies(&mut cs); cs });
                        }
                        // 行源 EOF：发送者已退出，关闭手动触发通道
                        None => { manual_open = false; }
                    }
                }
                _ = tick.tick() => {
                    match poll_once.await {
                        Err(()) => {
                            consecutive_errors += 1;
                            if consecutive_errors >= CONSECUTIVE_ERROR_LIMIT {
                                return Err(AppError::new(
                                    Code::Cancelled,
                                    Stage::LoginBrowser,
                                    "浏览器窗口已被关闭，登录取消",
                                ));
                            }
                        }
                        Ok(cookies) => {
                            if watch.poll(&cookies) == Poll::Detected {
                                return Ok(cookies);
                            }
                        }
                    }
                }
            }
        } else {
            tokio::select! {
                _ = &mut cancel_fut => {
                    return Err(AppError::new(Code::Cancelled, Stage::LoginBrowser, "登录已取消"));
                }
                _ = tick.tick() => {
                    match poll_once.await {
                        Err(()) => {
                            consecutive_errors += 1;
                            if consecutive_errors >= CONSECUTIVE_ERROR_LIMIT {
                                return Err(AppError::new(
                                    Code::Cancelled,
                                    Stage::LoginBrowser,
                                    "浏览器窗口已被关闭，登录取消",
                                ));
                            }
                        }
                        Ok(cookies) => {
                            if watch.poll(&cookies) == Poll::Detected {
                                return Ok(cookies);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn sort_cookies(cookies: &mut [CookieEntry]) {
    cookies.sort_by(|a, b| a.name.cmp(&b.name));
}

/// 把采集到的 cookie 集合转成已校验的凭证：cookie 头 + 浏览器自己的
/// User-Agent（唯一值得保留的非敏感头）。
async fn build_credentials(
    session: &mut dyn BrowserSession,
    cookies: &[CookieEntry],
) -> Result<Credentials> {
    if cookies.is_empty() {
        return Err(AppError::new(
            Code::AuthRequired,
            Stage::LoginCapture,
            "未采集到元宝 Cookie；请重新执行 sph login 并完成登录",
        ));
    }
    let parts: Vec<String> = cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    let mut headers = std::collections::BTreeMap::new();
    if let Ok(ua) = session.user_agent().await {
        if !ua.is_empty() {
            headers.insert("user-agent".to_string(), ua);
        }
    }
    let filtered = auth::validate_header_map(&headers).map_err(|_| {
        AppError::new(
            Code::LoginCaptureFailed,
            Stage::LoginCapture,
            "采集到的请求头未通过安全校验",
        )
    })?;
    let creds = Credentials {
        saved_at: OffsetDateTime::now_utc(),
        source: Source::BrowserLogin,
        verified_at: None,
        cookie: parts.join("; "),
        yuanbao_headers: filtered,
    };
    creds.validate().map_err(|_| {
        AppError::new(
            Code::LoginCaptureFailed,
            Stage::LoginCapture,
            "采集到的凭证未通过格式校验",
        )
    })?;
    Ok(creds)
}

fn make_work_dir(config_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    use rand::Rng;
    let mut buf = [0u8; 5];
    rand::thread_rng().fill(&mut buf);
    let mut hex = String::with_capacity(10);
    for b in buf {
        hex.push_str(&format!("{b:02x}"));
    }
    let dir = config_dir.join(format!(".login-{hex}"));
    std::fs::create_dir(&dir).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::LoginBrowser,
            format_args!("无法创建登录工作目录: {e}"),
        )
    })?;
    Ok(dir)
}

fn work_dir_str(p: &std::path::Path) -> &str {
    p.to_str().unwrap_or(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const SENTINEL: &str = "DO_NOT_LEAK_COOKIE_123";

    struct FakeBrowser {
        script: Mutex<VecDeque<Vec<CookieEntry>>>,
        poll: Mutex<usize>,
        ua: String,
        broken: Mutex<bool>,
        closed: Mutex<bool>,
    }

    impl FakeBrowser {
        fn new(script: Vec<Vec<CookieEntry>>, ua: &str) -> FakeBrowser {
            FakeBrowser {
                script: Mutex::new(script.into_iter().collect()),
                poll: Mutex::new(0),
                ua: ua.into(),
                broken: Mutex::new(false),
                closed: Mutex::new(false),
            }
        }

        fn arm_broken(&self) {
            *self.broken.lock().unwrap() = true;
        }

        fn was_closed(&self) -> bool {
            *self.closed.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl BrowserSession for FakeBrowser {
        async fn cookies(&self, _target: &str) -> std::result::Result<Vec<CookieEntry>, String> {
            if *self.broken.lock().unwrap() {
                return Err("browser is closed".into());
            }
            let s = self.script.lock().unwrap();
            let mut p = self.poll.lock().unwrap();
            if s.is_empty() {
                return Ok(vec![]);
            }
            let idx = (*p).min(s.len() - 1);
            *p += 1;
            Ok(s.get(idx).cloned().unwrap_or_default())
        }

        async fn user_agent(&self) -> std::result::Result<String, String> {
            if self.ua.is_empty() {
                Err("no ua".into())
            } else {
                Ok(self.ua.clone())
            }
        }

        async fn close(&mut self) {
            *self.closed.lock().unwrap() = true;
        }
    }

    struct Lines {
        lines: Vec<String>,
        pos: usize,
        block: Option<Duration>,
    }

    impl LineSource for Lines {
        fn read_line(&mut self) -> Option<String> {
            if let Some(d) = self.block {
                std::thread::sleep(d);
                self.block = None;
                return None;
            }
            if self.pos >= self.lines.len() {
                return None;
            }
            let line = self.lines[self.pos].clone();
            self.pos += 1;
            Some(line)
        }
    }

    fn guest() -> Vec<CookieEntry> {
        vec![
            CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            },
            CookieEntry {
                name: "guest_b".into(),
                value: "2".into(),
            },
        ]
    }

    fn logged_in() -> Vec<CookieEntry> {
        vec![
            CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            },
            CookieEntry {
                name: "guest_b".into(),
                value: "2".into(),
            },
            CookieEntry {
                name: "session".into(),
                value: SENTINEL.into(),
            },
            CookieEntry {
                name: "token".into(),
                value: "t".into(),
            },
        ]
    }

    fn default_script() -> Vec<Vec<CookieEntry>> {
        vec![
            guest(),
            guest(),
            guest(),
            guest(),
            logged_in(),
            logged_in(),
            logged_in(),
            logged_in(),
        ]
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-login-test-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    struct Env {
        store: auth::Store,
        session: Arc<FakeBrowser>,
        stderr: Arc<Mutex<Vec<u8>>>,
        deps: Options,
    }

    fn new_env() -> Env {
        let base = tempdir();
        let store = auth::Store::new(base.join("cfg")).unwrap();
        let session = Arc::new(FakeBrowser::new(default_script(), "UA-TEST/1"));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let session2 = session.clone();
        let stderr2 = stderr.clone();
        let deps = Options {
            timeout: Duration::from_secs(60),
            stderr: stderr.clone(),
            launch: Some(Arc::new(move |_work_dir, _err| {
                let s = session2.clone();
                let f: futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>> =
                    Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) });
                f
            })),
            now: Some(Arc::new(|| {
                time::OffsetDateTime::from_unix_timestamp(1758187200).unwrap()
            })),
            stdin: Some(Box::new(Lines {
                lines: vec![],
                pos: 0,
                block: None,
            })),
            cancel: None,
        };
        let _ = stderr2;
        Env {
            store,
            session,
            stderr,
            deps,
        }
    }

    /// 把 Arc<FakeBrowser> 包成可 close 的会话（close 标记写回共享状态）。
    struct ArcBrowser(Arc<FakeBrowser>);

    #[async_trait::async_trait]
    impl BrowserSession for ArcBrowser {
        async fn cookies(&self, target: &str) -> std::result::Result<Vec<CookieEntry>, String> {
            self.0.cookies(target).await
        }
        async fn user_agent(&self) -> std::result::Result<String, String> {
            self.0.user_agent().await
        }
        async fn close(&mut self) {
            *self.0.closed.lock().unwrap() = true;
        }
    }

    #[tokio::test]
    async fn login_saves_credentials() {
        let Env {
            store,
            session,
            stderr,
            deps,
        } = new_env();
        run(&store, true, deps).await.unwrap();
        let creds = store.load().unwrap();
        assert_eq!(creds.source, Source::BrowserLogin);
        assert!(
            creds.cookie.contains(&format!("session={SENTINEL}")),
            "cookie={}",
            creds.cookie
        );
        assert_eq!(creds.verified_at, None);
        assert_eq!(creds.yuanbao_headers["user-agent"], "UA-TEST/1");
        assert!(session.was_closed(), "浏览器必须被关闭");
        let out = String::from_utf8_lossy(&stderr.lock().unwrap()).into_owned();
        assert!(!out.contains(SENTINEL), "stderr 泄漏了 cookie");
        assert!(out.contains("已保存"));
    }

    #[tokio::test]
    async fn non_interactive() {
        let env = new_env();
        let err = run(&env.store, false, env.deps).await.unwrap_err();
        assert_eq!(err.code, Code::InteractiveRequired);
    }

    #[tokio::test]
    async fn launch_failure_saves_nothing() {
        let env = new_env();
        let mut deps = env.deps;
        deps.launch = Some(Arc::new(|_w, _e| {
            Box::pin(async {
                Err(AppError::new(
                    Code::LoginBrowserFailed,
                    Stage::LoginBrowser,
                    "无法启动登录浏览器",
                ))
            }) as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        let err = run(&env.store, true, deps).await.unwrap_err();
        assert_eq!(err.code, Code::LoginBrowserFailed);
        assert!(!env.store.exists());
    }

    #[tokio::test]
    async fn lock_busy() {
        let env = new_env();
        let lock = env.store.acquire_lock().unwrap();
        let err = run(&env.store, true, env.deps).await.unwrap_err();
        assert_eq!(err.code, Code::AuthBusy);
        lock.release().unwrap();
    }

    #[tokio::test]
    async fn capture_failure_keeps_old_credentials() {
        let env = new_env();
        let old = Credentials {
            saved_at: time::OffsetDateTime::now_utc(),
            source: Source::ManualImport,
            verified_at: None,
            cookie: "old=1".into(),
            yuanbao_headers: Default::default(),
        };
        env.store.save(&old).unwrap();
        let before = std::fs::read(env.store.path()).unwrap();
        // 空 jar + 手动 Enter → 采集不到任何 cookie
        let session = Arc::new(FakeBrowser::new(vec![], "UA-TEST/1"));
        let session2 = session.clone();
        let mut deps = env.deps;
        deps.launch = Some(Arc::new(move |_w, _e| {
            let s = session2.clone();
            Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) })
                as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        deps.stdin = Some(Box::new(Lines {
            lines: vec![String::new()],
            pos: 0,
            block: None,
        }));
        let err = run(&env.store, true, deps).await.unwrap_err();
        assert_eq!(err.code, Code::AuthRequired);
        let after = std::fs::read(env.store.path()).unwrap();
        assert_eq!(before, after, "失败的登录不得覆盖旧凭证");
        assert!(session.was_closed(), "失败路径也必须关闭浏览器");
    }

    #[tokio::test]
    async fn manual_enter_fallback() {
        let env = new_env();
        // 恒定 logged-in jar：自动检测无法触发（无增长），手动 Enter 采集此刻的 jar
        let session = Arc::new(FakeBrowser::new(vec![logged_in()], "UA-TEST/1"));
        let session2 = session.clone();
        let mut deps = env.deps;
        deps.launch = Some(Arc::new(move |_w, _e| {
            let s = session2.clone();
            Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) })
                as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        deps.stdin = Some(Box::new(Lines {
            lines: vec![String::new()],
            pos: 0,
            block: None,
        }));
        run(&env.store, true, deps).await.unwrap();
        let creds = env.store.load().unwrap();
        assert!(
            creds.cookie.contains(&format!("session={SENTINEL}")),
            "cookie={}",
            creds.cookie
        );
    }

    #[tokio::test]
    async fn timeout_waiting_for_login() {
        let env = new_env();
        let session = Arc::new(FakeBrowser::new(
            vec![vec![CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            }]],
            "UA",
        ));
        let session2 = session.clone();
        let mut deps = env.deps;
        deps.timeout = Duration::from_millis(200);
        deps.launch = Some(Arc::new(move |_w, _e| {
            let s = session2.clone();
            Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) })
                as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        deps.stdin = Some(Box::new(Lines {
            lines: vec![],
            pos: 0,
            block: Some(Duration::from_secs(2)),
        }));
        let err = run(&env.store, true, deps).await.unwrap_err();
        assert_eq!(err.code, Code::Timeout);
        assert!(session.was_closed(), "超时必须关闭浏览器");
    }

    #[tokio::test]
    async fn cancel_during_wait() {
        let env = new_env();
        let session = Arc::new(FakeBrowser::new(
            vec![vec![CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            }]],
            "UA",
        ));
        let session2 = session.clone();
        let cancel = crate::http::CancelToken::default();
        let mut deps = env.deps;
        deps.cancel = Some(cancel.clone());
        deps.launch = Some(Arc::new(move |_w, _e| {
            let s = session2.clone();
            Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) })
                as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        deps.stdin = Some(Box::new(Lines {
            lines: vec![],
            pos: 0,
            block: Some(Duration::from_secs(5)),
        }));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel.cancel();
        });
        let err = run(&env.store, true, deps).await.unwrap_err();
        assert_eq!(err.code, Code::Cancelled);
    }

    #[tokio::test]
    async fn browser_closed_by_user() {
        let env = new_env();
        let session = Arc::new(FakeBrowser::new(
            vec![vec![CookieEntry {
                name: "guest_a".into(),
                value: "1".into(),
            }]],
            "UA",
        ));
        let session2 = session.clone();
        let mut deps = env.deps;
        deps.launch = Some(Arc::new(move |_w, _e| {
            let s = session2.clone();
            let s2 = session2.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1200));
                s2.arm_broken();
            });
            Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) })
                as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        deps.stdin = Some(Box::new(Lines {
            lines: vec![],
            pos: 0,
            block: Some(Duration::from_secs(10)),
        }));
        let err = run(&env.store, true, deps).await.unwrap_err();
        assert_eq!(err.code, Code::Cancelled);
        assert!(
            format!("{err}").contains("浏览器窗口已被关闭"),
            "错误应归因于窗口关闭: {err}"
        );
    }

    #[tokio::test]
    async fn no_secrets_in_output() {
        let env = new_env();
        let stderr_for_check = env.stderr.clone();
        let script = vec![
            vec![],
            vec![],
            vec![],
            vec![CookieEntry {
                name: "session".into(),
                value: SENTINEL.into(),
            }],
            vec![CookieEntry {
                name: "session".into(),
                value: SENTINEL.into(),
            }],
            vec![CookieEntry {
                name: "session".into(),
                value: SENTINEL.into(),
            }],
        ];
        let session = Arc::new(FakeBrowser::new(script, "UA"));
        let session2 = session.clone();
        let mut deps = env.deps;
        deps.launch = Some(Arc::new(move |_w, _e| {
            let s = session2.clone();
            Box::pin(async move { Ok(Box::new(ArcBrowser(s)) as Box<dyn BrowserSession>) })
                as futures::future::BoxFuture<'static, Result<Box<dyn BrowserSession>>>
        }));
        run(&env.store, true, deps).await.unwrap();
        let out = String::from_utf8_lossy(&stderr_for_check.lock().unwrap()).into_owned();
        assert!(!out.contains(SENTINEL), "stderr 泄漏了 cookie 值");
    }
}
