//! 浏览器自动化基础设施：通用启动器、浏览器获取。
//! （M3 起承载 page_state / Jev / LLM 恢复层。）

pub mod fetch;
pub mod page_state;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

use crate::apperr::{AppError, Code, Result, Stage};

pub type SharedWriter = Arc<Mutex<dyn Write + Send>>;

/// 通用启动：持久或一次性 profile，headless 或可见。
pub async fn launch(
    chrome_path: &Path,
    profile_dir: &Path,
    headed: bool,
) -> Result<(Browser, chromiumoxide::Page)> {
    // 视口：chromiumoxide 默认对每个页面下发 800x600 设备仿真（页面布局锁死、
    // 窗口拉大出现大片空白），必须显式覆盖为合理尺寸。
    let viewport = chromiumoxide::handler::viewport::Viewport {
        width: 1440,
        height: 900,
        device_scale_factor: None,
        emulating_mobile: false,
        is_landscape: true,
        has_touch: false,
    };
    let mut builder = BrowserConfig::builder()
        .chrome_executable(chrome_path)
        .user_data_dir(profile_dir)
        .window_size(1440, 900)
        .viewport(viewport)
        // 库默认参数带 --enable-automation（"自动化软件控制"横幅）与 --lang=en_US
        // （触发 Google 翻译气泡），整体禁用后按需自管。
        .disable_default_args()
        .arg("--lang=zh-CN")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-infobars")
        .arg("--disable-features=Translate,TranslateUI")
        .arg("--disable-session-crashed-bubble")
        .arg("--hide-crash-restore-bubble")
        .arg("--disable-background-networking")
        .arg("--disable-background-timer-throttling")
        .arg("--disable-backgrounding-occluded-windows")
        .arg("--disable-breakpad")
        .arg("--disable-client-side-phishing-detection")
        .arg("--disable-component-extensions-with-background-pages")
        .arg("--disable-default-apps")
        .arg("--disable-dev-shm-usage")
        .arg("--disable-hang-monitor")
        .arg("--disable-ipc-flooding-protection")
        .arg("--disable-popup-blocking")
        .arg("--disable-prompt-on-repost")
        .arg("--disable-renderer-backgrounding")
        .arg("--disable-sync")
        .arg("--metrics-recording-only")
        .arg("--password-store=basic")
        .arg("--use-mock-keychain");
    // CI/Linux 环境禁用 sandbox：Ubuntu 23.10+（含 GitHub runner）默认
    // AppArmor 禁止非特权用户命名空间，Chromium 无法以 sandbox 启动。
    // 本机真实使用保持 sandbox 开启。
    if std::env::var_os("CI").is_some() || std::env::var_os("SPH_NO_SANDBOX").is_some() {
        builder = builder.no_sandbox();
    }
    if headed {
        builder = builder.with_head();
    }
    let cfg = builder.build().map_err(|e| {
        AppError::fmt(
            Code::LoginBrowserFailed,
            Stage::LoginBrowser,
            format_args!("无法配置浏览器: {e}"),
        )
    })?;
    let (browser, mut handler) = Browser::launch(cfg).await.map_err(|e| {
        AppError::fmt(
            Code::LoginBrowserFailed,
            Stage::LoginBrowser,
            format_args!("无法启动浏览器: {e}"),
        )
    })?;
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.map_err(|e| {
        AppError::fmt(
            Code::LoginBrowserFailed,
            Stage::LoginBrowser,
            format_args!("无法打开新页面: {e}"),
        )
    })?;
    Ok((browser, page))
}

/// 等待页面出现给定 selector（固定流程原语；失败路径留给 M3 恢复层）。
pub async fn wait_for_selector(
    page: &chromiumoxide::Page,
    selector: &str,
    timeout: Duration,
    stage: Stage,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match page.find_element(selector).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return Err(AppError::new(Code::Timeout, stage, "等待页面元素超时"));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}
