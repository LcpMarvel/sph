//! chromiumoxide 覆盖度 spike：验证 M1/M2 浏览器层所需的 CDP 能力。
//! 用法：cargo run [-- CHROME_PATH]

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::{SetDownloadBehaviorBehavior, SetDownloadBehaviorParams};
use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::cdp::browser_protocol::network::SetCookieParams;
use chromiumoxide::cdp::browser_protocol::page::EventLoadEventFired;
use chromiumoxide::cdp::browser_protocol::storage::GetCookiesParams;
use futures::StreamExt;

struct Probe {
    name: &'static str,
    ok: bool,
    detail: String,
}

async fn probe_async<F, Fut>(name: &'static str, f: F) -> Probe
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let (ok, detail) = match tokio::time::timeout(Duration::from_secs(20), f()).await {
        Ok(Ok(d)) => (true, d),
        Ok(Err(d)) => (false, d),
        Err(_) => (false, "timeout 20s".into()),
    };
    Probe { name, ok, detail }
}

#[tokio::main]
async fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let chrome = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/lcp/.cache/rod/browser/chromium-1321438/Chromium.app/Contents/MacOS/Chromium".into());
    let fixture = format!("file://{}", root.join("fixtures/upload.html").display());
    let dl_dir = root.join("out");
    std::fs::create_dir_all(&dl_dir).expect("create out dir");
    let upload_file = root.join("fixtures/upload.html"); // 拿 fixture 自己当上传文件

    let mut results: Vec<Probe> = Vec::new();

    // ── 1. headless 启动（失败则全部终止）─────────────────────
    let mut browser = match Browser::launch(
        BrowserConfig::builder()
            .chrome_executable(&chrome)
            .build()
            .expect("config"),
    )
    .await
    {
        Ok((browser, mut handler)) => {
            tokio::spawn(async move { while handler.next().await.is_some() {} });
            results.push(Probe {
                name: "launch: headless chromium 启动",
                ok: true,
                detail: format!("via {chrome}"),
            });
            browser
        }
        Err(e) => {
            println!("[FAIL] launch: headless chromium 启动 — {e}");
            println!("---\n0 passed, 1 failed");
            std::process::exit(1);
        }
    };

    let page = browser.new_page("about:blank").await.expect("new_page");

    // ── 2. 页面事件：LoadEventFired 监听 ─────────────────────
    results.push(
        probe_async("events: Page.loadEventFired 可订阅可接收", || {
            let page = &page;
            let fixture = fixture.clone();
            async move {
                let mut events = page
                    .event_listener::<EventLoadEventFired>()
                    .await
                    .map_err(|e| e.to_string())?;
                page.goto(fixture).await.map_err(|e| e.to_string())?;
                tokio::time::timeout(Duration::from_secs(10), events.next())
                    .await
                    .map_err(|_| "event not received in 10s".to_string())?
                    .ok_or("event stream closed".to_string())?;
                Ok("load event received".into())
            }
        })
        .await,
    );

    // ── 3. Cookie 观察：setCookie 后 Storage.getCookies 轮询可见 ──
    results.push(
        probe_async("cookies: Network.setCookie → Storage.getCookies 轮询可见（登录状态机核心原语）", || {
            let page = &page;
            async move {
                let baseline = page
                    .execute(GetCookiesParams::default())
                    .await
                    .map_err(|e| e.to_string())?;
                let before = baseline.cookies.len();
                let _set = page
                    .execute(
                        SetCookieParams::builder()
                            .name("spike_probe")
                            .value("1")
                            .domain(".example.com")
                            .build()
                            .map_err(|e| e)?,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let after = page
                    .execute(GetCookiesParams::default())
                    .await
                    .map_err(|e| e.to_string())?;
                let found = after.cookies.iter().any(|c| c.name == "spike_probe");
                if !found {
                    return Err("cookie not visible after set".into());
                }
                Ok(format!(
                    "baseline {before} → {} cookies, spike_probe visible",
                    after.cookies.len()
                ))
            }
        })
        .await,
    );

    // ── 4. DOM 查询 + JS 求值 ────────────────────────────────
    results.push(
        probe_async("dom/js: find_element + evaluate", || {
            let page = &page;
            async move {
                let el = page.find_element("#marker").await.map_err(|e| e.to_string())?;
                let text = el.inner_text().await.map_err(|e| e.to_string())?;
                let title = page.evaluate("document.title").await.map_err(|e| e.to_string())?;
                Ok(format!("marker={text:?}, title={title:?}"))
            }
        })
        .await,
    );

    // ── 5. 文件上传：DOM.setFileInputFiles（发布核心能力）────
    results.push(
        probe_async("upload: DOM.setFileInputFiles via node_id + Page::execute", || {
            let page = &page;
            let upload_file = upload_file.clone();
            async move {
                let el = page
                    .find_element("input[type=file]")
                    .await
                    .map_err(|e| e.to_string())?;
                page.execute(
                    SetFileInputFilesParams::builder()
                        .files(vec![upload_file.to_string_lossy().into_owned()])
                        .node_id(el.node_id)
                        .build()
                        .map_err(|e| e)?,
                )
                .await
                .map_err(|e| e.to_string())?;
                let n = page
                    .evaluate("document.getElementById('file').files.length")
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(format!("files.length = {:?}", n.value()))
            }
        })
        .await,
    );

    // ── 6. 浏览器下载：Browser.setDownloadBehavior + 真实落盘 ─
    results.push(
        probe_async("download: Browser.setDownloadBehavior + a[download] 落盘", || {
            let page = &page;
            let dl_dir = dl_dir.clone();
            async move {
                page.execute(
                    SetDownloadBehaviorParams::builder()
                        .behavior(SetDownloadBehaviorBehavior::Allow)
                        .download_path(dl_dir.to_string_lossy())
                        .build()
                        .map_err(|e| e)?,
                )
                .await
                .map_err(|e| e.to_string())?;
                page.find_element("#dl").await.map_err(|e| e.to_string())?.click().await.map_err(|e| e.to_string())?;
                let target = dl_dir.join("spike.txt");
                for _ in 0..20 {
                    if target.exists() {
                        let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                        return Ok(format!("spike.txt landed, {bytes} bytes"));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err("spike.txt not found after 5s".into())
            }
        })
        .await,
    );

    let _ = browser.close().await;

    // ── 7. headed 启动（login 需要可见浏览器扫码）────────────
    results.push(
        probe_async("headed: with_head() 可见浏览器启动并关闭", || {
            let chrome = chrome.clone();
            async move {
                let cfg = BrowserConfig::builder()
                    .chrome_executable(&chrome)
                    .with_head()
                    .build()
                    .map_err(|e| e.to_string())?;
                let (mut browser, mut handler) = Browser::launch(cfg).await.map_err(|e| e.to_string())?;
                tokio::spawn(async move { while handler.next().await.is_some() {} });
                let page = browser.new_page("about:blank").await.map_err(|e| e.to_string())?;
                tokio::time::sleep(Duration::from_millis(800)).await;
                drop(page);
                browser.close().await.map_err(|e| e.to_string())?;
                Ok("headed launch + close ok".into())
            }
        })
        .await,
    );

    // ── 汇总 ────────────────────────────────────────────────
    let mut failed = 0;
    for p in &results {
        println!("[{}] {} — {}", if p.ok { "PASS" } else { "FAIL" }, p.name, p.detail);
        if !p.ok {
            failed += 1;
        }
    }
    println!("---");
    println!("{} passed, {} failed", results.len() - failed, failed);
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
