//! 视频管理：已发布视频列表（list）与元信息修改（edit）。

pub mod api;
pub mod edit;
pub mod list;
pub mod page;
pub mod patches;

use std::path::Path;
use std::time::Duration;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::{self, fetch, SharedWriter};
use crate::publish::mod_impl::{navigate, OpenPageFn, OpenedPage};
use crate::session::{self, AccountDir};
use crate::videos::page::{Selectors, VideoManagePage, DEFAULT_SELECTORS};
use crate::videos::patches::ApiConfig;

pub use api::VideoEntry;
pub use edit::{run_edit, validate_edit, EditOptions, EditResult};
pub use list::{run_list, ListOptions, ListResult};

#[cfg(test)]
mod e2e_tests;

/// 会话引导完成后的页面上下文（browser 句柄保活；析构前必须 close）。
pub(crate) struct SessionPage {
    pub browser: Option<chromiumoxide::browser::Browser>,
    pub page: chromiumoxide::Page,
    pub selectors: Selectors,
    pub api: ApiConfig,
}

impl SessionPage {
    /// 优雅关闭：强杀会让持久 profile 的会话变更来不及落盘。
    pub(crate) async fn close(mut self) {
        if let Some(b) = self.browser.as_mut() {
            let _ = b.close().await;
        }
        drop(self.browser);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn werr(stderr: &SharedWriter, args: std::fmt::Arguments) {
    if let Ok(mut g) = stderr.lock() {
        let _ = g.write_fmt(args);
        let _ = g.flush();
    }
}

/// list/edit 共用引导：会话检查 → 补丁合并 → 浏览器 → 导航主页 → 就绪 + 弹窗处理。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn bootstrap(
    config_dir: &Path,
    account_name: &str,
    headed: bool,
    open_page: Option<OpenPageFn>,
    navigate_url: Option<&str>,
    step_timeout: Duration,
    cancel: &crate::http::CancelToken,
    stderr: &SharedWriter,
    selectors_override: Option<Selectors>,
) -> Result<SessionPage> {
    let account_dir = AccountDir::new(&session::accounts_root(config_dir), account_name);
    if !account_dir.profile_exists() {
        return Err(AppError::fmt(
            Code::SessionExpired,
            Stage::SessionLoad,
            format_args!("账号 {account_name} 没有已保存的助手会话，请执行 sph login。"),
        ));
    }
    let profile_dir = account_dir.profile_dir();
    let (selectors, api) = match selectors_override {
        Some(custom) => (custom, patches::DEFAULT_API.clone()),
        None => patches::load_and_merge(config_dir, &DEFAULT_SELECTORS)?,
    };

    werr(
        stderr,
        format_args!("正在加载视频号助手会话（账号: {account_name}）…\n"),
    );
    let opened: OpenedPage = match open_page {
        Some(open) => open(&profile_dir, headed).await?,
        None => {
            let chrome = fetch::ensure_chromium(stderr.clone()).await?;
            let (b, p) = browser::launch(&chrome, &profile_dir, headed).await?;
            OpenedPage {
                browser: Some(b),
                page: p,
            }
        }
    };

    let home_url = navigate_url.unwrap_or(session::ASSISTANT_HOME);
    navigate(&opened.page, home_url, cancel).await?;
    let flow = VideoManagePage::new(&opened.page, &selectors);
    flow.require_logged_in().await?;
    flow.wait_home_ready(step_timeout).await?;
    for _ in 0..3 {
        if !flow.dismiss_dialogs().await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    Ok(SessionPage {
        browser: opened.browser,
        page: opened.page,
        selectors,
        api,
    })
}
