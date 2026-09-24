//! 生产登录浏览器：chromiumoxide 驱动的专用可见 Chromium + 私有 profile。

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::GetCookiesParams;
use futures::StreamExt;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::fetch;

use super::{BrowserSession, CookieEntry};

const HOME_PAGE: &str = "https://yuanbao.tencent.com/";

type SharedWriter = Arc<Mutex<dyn Write + Send>>;

pub struct ChromiumSession {
    browser: Browser,
    page: chromiumoxide::Page,
}

/// 启动专用可见浏览器实例，私有 profile 位于登录工作目录下。
/// 首次使用自动下载专用 Chromium（约 200MB，仅此一次，多镜像竞速）。
pub async fn launch(work_dir: &str, stderr: SharedWriter) -> Result<ChromiumSession> {
    let chrome_path = fetch::ensure_chromium(stderr.clone()).await?;

    let profile_dir = std::path::Path::new(work_dir).join("browser-profile");
    let cfg = BrowserConfig::builder()
        .chrome_executable(&chrome_path)
        .user_data_dir(&profile_dir)
        .arg("--window-size=1280,860")
        .with_head()
        .build()
        .map_err(|e| {
            AppError::fmt(
                Code::LoginBrowserFailed,
                Stage::LoginBrowser,
                format_args!("无法配置登录浏览器: {e}"),
            )
        })?;

    let (browser, mut handler) = Browser::launch(cfg).await.map_err(|e| {
        AppError::fmt(
            Code::LoginBrowserFailed,
            Stage::LoginBrowser,
            format_args!("无法启动登录浏览器；首次使用需要联网下载浏览器，请检查网络后重试: {e}"),
        )
    })?;
    tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page(HOME_PAGE).await.map_err(|e| {
        AppError::fmt(
            Code::LoginBrowserFailed,
            Stage::LoginBrowser,
            format_args!("无法打开元宝官网页面: {e}"),
        )
    })?;
    // 页面有界等待；登录检测只依赖 cookie jar，页面慢不是致命问题
    let _ = tokio::time::timeout(Duration::from_secs(30), page.wait_for_navigation()).await;

    Ok(ChromiumSession { browser, page })
}

#[async_trait::async_trait]
impl BrowserSession for ChromiumSession {
    async fn cookies(&self, target_url: &str) -> std::result::Result<Vec<CookieEntry>, String> {
        let resp = self
            .page
            .execute(GetCookiesParams {
                urls: Some(vec![target_url.to_string()]),
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

    async fn user_agent(&self) -> std::result::Result<String, String> {
        let res = self
            .page
            .evaluate("navigator.userAgent")
            .await
            .map_err(|e| e.to_string())?;
        let ua = res
            .value()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if ua.is_empty() {
            return Err("empty user agent".into());
        }
        Ok(ua)
    }

    async fn close(&mut self) {
        let _ = self.browser.close().await;
    }
}
