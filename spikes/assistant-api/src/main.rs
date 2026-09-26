//! 视频号助手内部 API spike：校准 list/edit 所需的真实端点与交互路径。
//!
//! 用法（一律 headed，需要已登录的真实 profile）：
//!   cargo run --manifest-path spikes/assistant-api/Cargo.toml -- <probe> [PROFILE_DIR] [CHROME_PATH]
//!
//! probe：
//!   route  验证 post/list 直达是否被路由守卫弹回主页
//!   sniff  Network 域抓视频管理页真实流量（人工在页面上点"修改"/翻页/筛选）
//!   fetch  页面上下文手工 fetch 候选 post_list 端点
//!   edit   dump 视频管理页行操作区与编辑表单控件（不提交任何修改）
//!
//! 产物写到 spikes/assistant-api/out/（gitignore 已覆盖，含敏感信息勿外发）。

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams, EventLoadingFinished, EventRequestWillBeSent, GetRequestPostDataParams,
    GetResponseBodyParams,
};
use chromiumoxide::Page;
use futures::StreamExt;

const HOME_URL: &str = "https://channels.weixin.qq.com/platform";
const POST_LIST_URL: &str = "https://channels.weixin.qq.com/platform/post/list";

fn default_profile() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".sph/accounts/default/profile")
}

fn default_chrome() -> PathBuf {
    if let Ok(p) = std::env::var("SPH_CHROME") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    for c in [
        home.join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string()),
        home.join("Applications/Chromium.app/Contents/MacOS/Chromium"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium".to_string()),
    ] {
        if c.exists() {
            return c;
        }
    }
    home.join(".cache/rod/browser/chromium-1321438/Chromium.app/Contents/MacOS/Chromium")
}

async fn launch(chrome: &PathBuf, profile: &PathBuf) -> (Browser, Page) {
    // SPH_SPIKE_HEADLESS=1 时无头运行（校准 headless 行为差异）
    let headless = std::env::var("SPH_SPIKE_HEADLESS").as_deref() == Ok("1");
    let mut builder = BrowserConfig::builder()
        .chrome_executable(chrome)
        .user_data_dir(profile)
        .window_size(1440, 900)
        .disable_default_args()
        .arg("--lang=zh-CN")
        .arg("--no-first-run")
        .arg("--no-default-browser-check");
    if !headless {
        builder = builder.with_head();
    }
    let cfg = builder.build().expect("browser config");
    let (browser, mut handler) = Browser::launch(cfg).await.expect("launch");
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.expect("new_page");
    (browser, page)
}

// ── edit6：headless 下编辑页导航校准 ────────────────────────────────────
//
// 实测（2026-09-26）：sph headless 全量加载直达 /platform/post/coverEdit
// 会被弹回 /platform（headed 不会）。本探针在 headless 下验证导航序列：
//   主页 → 侧边栏「视频」(SPA 内跳转) → coverEdit 内部跳转的三种方式：
//   a) 整页 goto；b) history.pushState + popstate；c) location.hash 式。
// 汇报每步 href 与 .edit-select-area 是否出现。

async fn probe_edit6(page: &Page) {
    let _ = page.goto(POST_LIST_URL.replace("/post/list", "").as_str()).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    println!("主页 href = {}", href(page).await);

    // 侧边栏「视频」菜单（SPA 内部跳转进视频管理）
    let clicked = eval_str(
        page,
        r#"(function(){
          const els = document.querySelectorAll('.finder-ui-desktop-menu__sub__li');
          for (const el of els) {
            if ((el.innerText || '').trim() === '视频') {
              el.dispatchEvent(new MouseEvent('click', {bubbles: true, cancelable: true}));
              return 'clicked';
            }
          }
          return 'no-menu';
        })()"#,
    )
    .await;
    println!("视频菜单: {clicked}");
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("菜单点击后 href = {}", href(page).await);

    // 拿 objectId
    let list_env = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":1,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    let marker = "\\\"objectId\\\":\\\"";
    let oid = list_env
        .find(marker)
        .map(|i| {
            list_env[i + marker.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_default();
    println!("objectId = {oid}");
    if oid.is_empty() {
        return;
    }
    let enc = oid.replace('/', "%2F");

    // 方式 b：pushState + popstate（SPA 内部）
    let r = eval_str(
        page,
        &format!(
            r#"(function(){{
          history.pushState({{}}, '', '/platform/post/coverEdit?objectId={enc}');
          window.dispatchEvent(new PopStateEvent('popstate'));
          return location.href;
        }})()"#
        ),
    )
    .await;
    println!("pushState 后 href = {r}");
    tokio::time::sleep(Duration::from_secs(6)).await;
    let found = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          for (const r of roots) { if (r.querySelector('.edit-select-area')) return 'y'; }
          return 'n';
        })()"#,
    )
    .await;
    println!("pushState 方式: href = {} 编辑页元素 = {found}", href(page).await);
    if found == "y" {
        return;
    }

    // 方式 c：行内「修改描述和封面」hover 入口点击（列表页内 SPA 跳转）
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let pos = find_pos(page, ".edit-cover-text", "修改描述和封面").await;
    println!("行内入口: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let found = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          for (const r of roots) { if (r.querySelector('.edit-select-area')) return 'y'; }
          return 'n';
        })()"#,
    )
    .await;
    println!("行内入口方式: href = {} 编辑页元素 = {found}", href(page).await);
}

async fn href(page: &Page) -> String {
    page.evaluate("location.href")
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default()
}

/// 等主页就绪标志（wujie shadowRoot + 主文档都查），超时返回 false。
async fn wait_home_ready(page: &Page, timeout: Duration) -> bool {
    let js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) { if (r.querySelector('.brand-name')) return true; }
      return false;
    })()"#;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(res) = page.evaluate(js).await {
            if matches!(res.value(), Some(serde_json::Value::Bool(true))) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── route：直达 post/list 是否被弹回主页 ──────────────────────

async fn probe_route(page: &Page) {
    println!("== 1) 直接 goto post/list ==");
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("5s 后 href = {}", href(page).await);
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("10s 后 href = {}", href(page).await);

    println!("== 2) 先着陆主页再 goto post/list ==");
    let _ = page.goto(HOME_URL).await;
    let ready = wait_home_ready(page, Duration::from_secs(30)).await;
    println!("主页就绪 = {ready}, href = {}", href(page).await);
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    println!("二次 goto 5s 后 href = {}", href(page).await);
}

// ── sniff：抓视频管理页真实 CGI 流量 ─────────────────────────

async fn probe_sniff(page: &Page, out_dir: &PathBuf) {
    page.execute(EnableParams::default()).await.expect("network enable");
    let mut requests = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .expect("request listener");
    let mut finished = page
        .event_listener::<EventLoadingFinished>()
        .await
        .expect("loading-finished listener");

    let log_path = out_dir.join(format!(
        "sniff-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    println!("抓包日志写到 {}", log_path.display());
    println!("导航到主页，请人工点进「内容管理/视频管理」，并点一次某条视频的「修改」、翻页、按合集筛选。抓 180s。");

    let _ = page.goto(HOME_URL).await;

    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut log_line = |s: String| {
        println!("{s}");
        log.lock().unwrap().push(s);
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    // (request_id 摘要, url) 缓存，供 loadingFinished 时取响应体
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    loop {
        let sleep = tokio::time::sleep(Duration::from_secs(3));
        tokio::pin!(sleep);
        tokio::select! {
            ev = requests.next() => {
                let Some(ev) = ev else { break };
                let url = ev.request.url.clone();
                if !(url.contains("cgi-bin") || url.contains("mmfinder")) { continue; }
                let mut line = format!("[REQ] {} {}", ev.request.method, url);
                if ev.request.has_post_data == Some(true) {
                    if let Ok(pd) = page.execute(GetRequestPostDataParams::new(ev.request_id.clone())).await {
                        let trimmed: String = pd.post_data.chars().take(500).collect();
                        line.push_str(&format!(" post_data={trimmed}"));
                    }
                }
                log_line(line);
                seen.insert(format!("{:?}", ev.request_id), url);
            }
            ev = finished.next() => {
                let Some(ev) = ev else { break };
                let key = format!("{:?}", ev.request_id);
                let Some(url) = seen.get(&key).cloned() else { continue };
                if let Ok(body) = page.execute(GetResponseBodyParams::new(ev.request_id.clone())).await {
                    let prefix: String = body.body.chars().take(2000).collect();
                    log_line(format!("[RESP] {url} body(base64={})={prefix}", body.base64_encoded));
                }
            }
            _ = &mut sleep => {
                if tokio::time::Instant::now() >= deadline { break; }
            }
        }
    }
    std::fs::write(&log_path, log.lock().unwrap().join("\n")).expect("write sniff log");
    println!("抓包结束。href = {}", href(page).await);
}

// ── fetch：页面上下文手工 fetch 候选端点 ─────────────────────

async fn probe_fetch(page: &Page) {
    let _ = page.goto(HOME_URL).await;
    let ready = wait_home_ready(page, Duration::from_secs(30)).await;
    println!("主页就绪 = {ready}, href = {}", href(page).await);
    if !ready {
        println!("未就绪，仍继续探测（可能未登录）");
    }

    let candidates = [
        // 分页参数定案：pageSize=1 + page=2 / currentPage=2，看哪个真正翻页
        ("/cgi-bin/mmfinderassistant-bin/post/post_list", r#"{"pageSize":1,"page":2}"#),
        ("/cgi-bin/mmfinderassistant-bin/post/post_list", r#"{"pageSize":1,"currentPage":2}"#),
    ];
    for (url, body) in candidates {
        let js = format!(
            r#"(async () => {{
              try {{
                const r = await fetch({url:?}, {{
                  method: 'POST',
                  headers: {{'Content-Type': 'application/json'}},
                  credentials: 'include',
                  body: {body:?}
                }});
                const j = JSON.parse(await r.text());
                const d = j.data || {{}};
                const item = (d.list || [])[0] || {{}};
                return JSON.stringify({{
                  status: r.status,
                  errCode: j.errCode,
                  dataKeys: Object.keys(d),
                  total: d.total, hasMore: d.hasMore, has_more: d.has_more,
                  itemKeys: Object.keys(item),
                  objectId: item.objectId,
                  createTime: item.createTime,
                  descKeys: item.desc ? Object.keys(item.desc) : null,
                  description: item.desc && item.desc.description,
                  title: item.title, shortTitle: item.shortTitle,
                  collectionInfo: item.collectionInfo, collection: item.collection,
                }});
              }} catch (e) {{ return 'FETCH_ERR ' + e; }}
            }})()"#
        );
        match page.evaluate(js).await {
            Ok(res) => {
                let v = res.value().cloned().unwrap_or(serde_json::Value::Null);
                let pretty = serde_json::from_str::<serde_json::Value>(
                    v.as_str().unwrap_or_default(),
                )
                .map(|j| serde_json::to_string_pretty(&j).unwrap_or_default())
                .unwrap_or_else(|_| v.to_string());
                println!("== POST {url} {body}\n{pretty}\n");
            }
            Err(e) => println!("== POST {url} {body}\nevaluate 失败: {e}\n"),
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

// ── collection：合集归属来源定案 ─────────────────────────────
//
// sniff（2026-09-26）抓到 post-card iframe 同时请求：
//   post/post_list            {"pageSize":5,"currentPage":1,"userpageType":11,"stickyOrder":false}
//   collection/get_collection_list  {"pageNum":1,"pageSize":20,"collectionScene":0,"collectionBusinessType":0}
// 响应 collectionList[] = {id:"event/UzFf...", name, feedCount}。
// 待回答：合集内视频清单来自哪里？两种假设都试：
//   A) post_list 支持 collectionId 过滤参数（合集 Tab 复用同一接口）
//   B) 独立 collection/get_collection_feed_list 类接口
// 判据：带合集 id 的 post_list 返回的 list 是否与不带参数的不同 / errCode 是否非 0。

const GET_COLLECTION_LIST: &str = "/cgi-bin/mmfinderassistant-bin/collection/get_collection_list";

async fn js_fetch(page: &Page, url: &str, body: &str) -> String {
    let js = format!(
        r#"(async () => {{
          try {{
            const r = await fetch({url:?}, {{
              method: 'POST',
              headers: {{'Content-Type': 'application/json'}},
              credentials: 'include',
              body: {body:?}
            }});
            return JSON.stringify({{status: r.status, body: (await r.text()).slice(0, 4096)}});
          }} catch (e) {{ return 'FETCH_ERR ' + e; }}
        }})()"#
    );
    match page.evaluate(js).await {
        Ok(res) => res
            .value()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default(),
        Err(e) => format!("EVAL_ERR {e}"),
    }
}

/// 从 get_collection_list 响应提取 (id, name, feedCount) 列表。
fn parse_collections(envelope: &str) -> Vec<(String, String, i64)> {
    let Ok(env) = serde_json::from_str::<serde_json::Value>(envelope) else {
        return vec![];
    };
    let body = env.get("body").and_then(|b| b.as_str()).unwrap_or_default();
    let Ok(j) = serde_json::from_str::<serde_json::Value>(body) else {
        return vec![];
    };
    j.get("data")
        .and_then(|d| d.get("collectionList"))
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    (
                        c.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        c.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        c.get("feedCount").and_then(|v| v.as_i64()).unwrap_or(-1),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// post_list 响应摘要：errCode + 前几条 objectId（用于判断过滤是否生效）。
fn summarize_post_list(envelope: &str) -> String {
    let Ok(env) = serde_json::from_str::<serde_json::Value>(envelope) else {
        return format!("非 JSON 信封: {}", &envelope[..envelope.len().min(200)]);
    };
    let status = env.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
    let body = env.get("body").and_then(|b| b.as_str()).unwrap_or_default();
    let Ok(j) = serde_json::from_str::<serde_json::Value>(body) else {
        return format!("HTTP {status}, 非 JSON 体: {}", &body[..body.len().min(200)]);
    };
    let err = j.get("errCode").and_then(|v| v.as_i64()).unwrap_or(-999);
    let ids: Vec<String> = j
        .get("data")
        .and_then(|d| d.get("list"))
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|it| it.get("objectId").and_then(|v| v.as_str()).map(String::from))
                .take(4)
                .collect()
        })
        .unwrap_or_default();
    let total = j
        .get("data")
        .and_then(|d| d.get("totalCount"))
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    format!("HTTP {status} errCode={err} totalCount={total} firstIds={ids:?}")
}

async fn probe_collection(page: &Page) {
    let _ = page.goto(HOME_URL).await;
    let ready = wait_home_ready(page, Duration::from_secs(30)).await;
    println!("主页就绪 = {ready}");

    // 1) 合集清单
    let list_env = js_fetch(
        page,
        GET_COLLECTION_LIST,
        r#"{"pageNum":1,"pageSize":20,"collectionScene":0,"collectionBusinessType":0}"#,
    )
    .await;
    let collections = parse_collections(&list_env);
    println!("== get_collection_list ==\n合集 {} 个:", collections.len());
    for (id, name, feed_count) in &collections {
        println!("  {name}  feedCount={feed_count}  id={id}");
    }
    let Some((cid, cname, _)) = collections.first() else {
        println!("无合集，探测结束");
        return;
    };
    println!("\n用合集「{cname}」(id={cid}) 试探归属接口：");

    // 2) 基线：无过滤的 post_list 首页
    let baseline = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":5,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    println!("[基线] post_list 无过滤: {}", summarize_post_list(&baseline));
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 3) 假设 A：post_list 带合集过滤参数（各种键名）
    for key in ["collectionId", "collection_id", "collection_id_str", "eventId"] {
        let body = format!(
            r#"{{"pageSize":5,"currentPage":1,"userpageType":11,"stickyOrder":false,"{key}":"{cid}"}}"#
        );
        let env = js_fetch(
            page,
            "/cgi-bin/mmfinderassistant-bin/post/post_list",
            &body,
        )
        .await;
        println!("[A:{key}] {}", summarize_post_list(&env));
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    // 4) 假设 B：独立合集内容接口（候选路径 × 候选请求体）
    let candidates = [
        ("/cgi-bin/mmfinderassistant-bin/collection/get_collection_feed_list", "collectionId"),
        ("/cgi-bin/mmfinderassistant-bin/collection/collection_feed_list", "collectionId"),
        ("/cgi-bin/mmfinderassistant-bin/collection/get_collection_post_list", "collectionId"),
        ("/cgi-bin/mmfinderassistant-bin/collection/get_collection_detail", "collectionId"),
        ("/cgi-bin/mmfinderassistant-bin/collection/get_collection_feed_list", "id"),
    ];
    for (url, id_key) in candidates {
        let body = format!(r#"{{"{id_key}":"{cid}","pageNum":1,"pageSize":5}}"#);
        let env = js_fetch(page, url, &body).await;
        println!("[B:{url} key={id_key}] {}", summarize_post_list(&env));
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    // 5) 命中接口定稿：get_collection_feed_list 的分页与字段形状
    let url = "/cgi-bin/mmfinderassistant-bin/collection/get_collection_feed_list";
    for page_num in [1, 2] {
        let body = format!(r#"{{"collectionId":"{cid}","pageNum":{page_num},"pageSize":2}}"#);
        let env = js_fetch(page, url, &body).await;
        let Ok(envj) = serde_json::from_str::<serde_json::Value>(&env) else {
            println!("[定稿 p{page_num}] 非信封: {env}");
            continue;
        };
        let body_text = envj.get("body").and_then(|b| b.as_str()).unwrap_or_default();
        // pageSize=2 的响应可能仍超 4096 截断；只解析前缀里能解析的部分 → 改用 JS 提取键
        let _ = body_text;
        let keys_js = format!(
            r#"(async () => {{
              const r = await fetch({url:?}, {{method:'POST', credentials:'include',
                headers:{{'Content-Type':'application/json'}}, body: {body:?}}});
              const j = JSON.parse(await r.text());
              const d = j.data || {{}};
              const oi = (d.objectInfo || [])[0] || {{}};
              const feed = oi.feed || {{}};
              return JSON.stringify({{
                status: r.status, errCode: j.errCode,
                dataKeys: Object.keys(d),
                objectInfoLen: (d.objectInfo || []).length,
                objectInfoKeys: Object.keys(oi),
                feedObjectId: feed.objectId,
                feedKeys: Object.keys(feed),
              }});
            }})()"#
        );
        match page.evaluate(keys_js).await {
            Ok(res) => println!(
                "[定稿 p{page_num}] {}",
                res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
            ),
            Err(e) => println!("[定稿 p{page_num}] evaluate 失败: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

// ── clickdbg：事件级诊断——埋监听后 CDP 点击，看哪些事件真正到达 ──

async fn probe_clickdbg(page: &Page) {
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 埋事件监听（捕获+冒泡都埋），并 scrollIntoView
    let instrument_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      window.__evlog = [];
      for (const r of roots) {
        for (const el of r.querySelectorAll('.edit-cover-text')) {
          el.scrollIntoView({block: 'center', inline: 'center'});
          const wrap = el.closest('.opr-item-wrap') || el;
          for (const target of [el, wrap, r]) {
            for (const t of ['pointerover','pointerdown','pointerup','mouseover','mousedown','mouseup','click']) {
              target.addEventListener(t, e => {
                window.__evlog.push(t + '@' + (e.target.className && e.target.className.slice ? e.target.className.slice(0,30) : e.target.tagName) + (e.composed ? ' composed' : ' NOCOMPOSE'));
              }, true);
            }
          }
          const b = el.getBoundingClientRect();
          return JSON.stringify({x: b.x + b.width/2, y: b.y + b.height/2});
        }
      }
      return '';
    })()"#;
    let pos = page
        .evaluate(instrument_js)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    println!("埋点: {pos}");
    let p: serde_json::Value = serde_json::from_str(&pos).expect("pos");
    let (x, y) = (
        p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
        p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
    );
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
    };
    let mv = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(x)
        .y(y)
        .build()
        .expect("mv");
    let _ = page.execute(mv).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    for t in [DispatchMouseEventType::MousePressed, DispatchMouseEventType::MouseReleased] {
        let params = DispatchMouseEventParams::builder()
            .r#type(t)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .expect("mouse params");
        let _ = page.execute(params).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    match page.evaluate("JSON.stringify(window.__evlog || [])").await {
        Ok(res) => println!("== 到达目标的事件 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("读事件日志失败: {e}"),
    }
    // 对照：合成 click 后是否有点击事件记录
    let _ = page
        .evaluate(
            r#"(function(){
              window.__evlog.push('---synthetic---');
              const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
              roots.push(document);
              for (const r of roots) {
                const el = r.querySelector('.edit-cover-item');
                if (el) { el.click(); return true; }
              }
              return false;
            })()"#,
        )
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    match page.evaluate("JSON.stringify(window.__evlog || [])").await {
        Ok(res) => println!("== 合成 click 后 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("读事件日志失败: {e}"),
    }

    // 点击后 6s 内每 500ms 轮询：任何 dialog/mask/drawer 类元素变为计算可见？
    //（click 确实到达但界面无变化——排查是否打开后瞬间因 JS 错误关闭，或检测选择器太窄）
    let _ = page
        .evaluate(
            r#"window.addEventListener('error', e => { (window.__errs = window.__errs || []).push(String(e.message).slice(0,200)); }, true)"#,
        )
        .await;
    let poll_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      const out = [];
      for (const r of roots) {
        for (const el of r.querySelectorAll('[class*=dialog], [class*=Dialog], [class*=drawer], [class*=modal]')) {
          const cs = getComputedStyle(el);
          if (cs.display === 'none' || cs.visibility === 'hidden' || cs.opacity === '0') continue;
          const b = el.getBoundingClientRect();
          if (b.width < 50 || b.height < 50) continue;
          if (typeof el.className !== 'string') continue;
          out.push(el.tagName + '.' + el.className.slice(0, 80) + ' ' + Math.round(b.width) + 'x' + Math.round(b.height));
          if (out.length >= 10) break;
        }
      }
      return out.join('\n');
    })()"#;
    for i in 0..12 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let vis = page
            .evaluate(poll_js)
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default();
        if !vis.is_empty() {
            println!("== t={}ms 可见对话框 ==\n{}", i * 500 + 500, vis);
            break;
        }
        if i == 11 {
            println!("== 6s 内无可见对话框 ==");
        }
    }
    match page.evaluate("JSON.stringify(window.__errs || [])").await {
        Ok(res) => println!("== 页面 JS 错误 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("读错误失败: {e}"),
    }
    // wujie 沙箱 iframe 排查：Vue Teleport 到 body 的弹窗可能渲染进了隐藏 iframe
    let iframe_js = r#"(function(){
      const out = [];
      const iframes = document.querySelectorAll('iframe');
      out.push('iframes=' + iframes.length);
      iframes.forEach((f, i) => {
        try {
          const d = f.contentDocument;
          if (!d) { out.push(`iframe[${i}] cross-origin`); return; }
          const b = f.getBoundingClientRect();
          const dialogs = d.querySelectorAll('[class*=dialog], [class*=Dialog], [class*=modal]');
          const inputs = d.querySelectorAll('input, textarea, [contenteditable=true]');
          out.push(`iframe[${i}] src=${(f.src||'').slice(0,60)} rect=${Math.round(b.width)}x${Math.round(b.height)} bodyChildren=${d.body ? d.body.children.length : -1} dialogs=${dialogs.length} inputs=${inputs.length}`);
          // 计算可见（display!=none 且有尺寸）的对话框全量 dump
          const visDlg = [];
          for (const el of dialogs) {
            if (typeof el.className !== 'string') continue;
            if (!el.className.includes('__wrp') && !el.className.includes('common-dialog')) continue;
            const cs = d.defaultView.getComputedStyle(el);
            const bb = el.getBoundingClientRect();
            if (cs.display === 'none') continue;
            visDlg.push(el.className.slice(0, 70) + ' ' + Math.round(bb.width) + 'x' + Math.round(bb.height));
          }
          out.push(`  visibleDialogs=${visDlg.length}: ${visDlg.join(' | ')}`);
          inputs.forEach((el, j) => {
            if (j < 12) out.push(`  input[${j}] ${el.tagName} type=${el.type||''} accept=${el.accept||''} ph=${el.getAttribute && el.getAttribute('placeholder') || ''} ce=${el.contentEditable||''} class=${(typeof el.className==='string'?el.className:'').slice(0,50)}`);
          });
        } catch (e) { out.push(`iframe[${i}] err ${e}`); }
      });
      return out.join('\n');
    })()"#;
    match page.evaluate(iframe_js).await {
        Ok(res) => println!("== iframe 排查 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("iframe 排查失败: {e}"),
    }
    if let Ok(bytes) = page
        .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
        .await
    {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("out/clickdbg-final.png");
        let _ = std::fs::write(&p, &bytes);
        println!("截图已存 {}", p.display());
    }
}


// ── edit2：走真实 UI 路径（主页 → CDP 点击视频菜单 → 列表 → 点击修改）──────
//
// 假设：直达 post/list 跳过了菜单导航时的初始化 CGI/状态，导致行内
// 「修改描述和封面」点击处理器空转（事件到达已证实，无可见变化无报错）。

async fn cdp_click(page: &Page, x: f64, y: f64) {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
    };
    let mv = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseMoved)
        .x(x)
        .y(y)
        .build()
        .expect("mv");
    let _ = page.execute(mv).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    for t in [DispatchMouseEventType::MousePressed, DispatchMouseEventType::MouseReleased] {
        let params = DispatchMouseEventParams::builder()
            .r#type(t)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .expect("mouse params");
        let _ = page.execute(params).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 在 shadow roots + 主文档中找第一个文本匹配且可见的元素中心坐标。
async fn find_pos(page: &Page, selector: &str, text: &str) -> Option<(f64, f64)> {
    let js = format!(
        r#"(function(){{
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
      for (const r of roots) {{
        for (const el of r.querySelectorAll({selector:?})) {{
          if (!vis(el)) continue;
          if ((el.innerText || '').trim() !== {text:?}) continue;
          el.scrollIntoView({{block: 'center', inline: 'center'}});
          const b = el.getBoundingClientRect();
          return JSON.stringify({{x: b.x + b.width/2, y: b.y + b.height/2}});
        }}
      }}
      return '';
    }})()"#
    );
    let s = page
        .evaluate(js)
        .await
        .ok()?
        .value()
        .and_then(|v| v.as_str().map(String::from))?;
    if s.is_empty() {
        return None;
    }
    let p: serde_json::Value = serde_json::from_str(&s).ok()?;
    Some((
        p.get("x").and_then(|v| v.as_f64())?,
        p.get("y").and_then(|v| v.as_f64())?,
    ))
}

async fn probe_edit2(page: &Page) {
    let _ = page.goto(HOME_URL).await;
    let ready = wait_home_ready(page, Duration::from_secs(30)).await;
    println!("主页就绪 = {ready}, href = {}", href(page).await);

    // 1) 点侧边栏「视频」菜单（selector 已校准：.finder-ui-desktop-menu__sub__li）
    let menu = find_pos(page, ".finder-ui-desktop-menu__sub__li", "视频").await;
    println!("视频菜单位置: {menu:?}");
    if let Some((x, y)) = menu {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_secs(6)).await;
    println!("菜单点击后 href = {}", href(page).await);

    // 2) 点第一行「修改描述和封面」
    let entry = find_pos(page, ".edit-cover-text", "修改描述和封面").await;
    println!("修改入口位置: {entry:?}");
    if let Some((x, y)) = entry {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let st = page
        .evaluate(DIALOG_OPEN_JS)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    println!("修改点击后对话框状态: {st}");
    if st == "open" {
        dump_edit_form(page).await;
    } else if let Ok(bytes) = page
        .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
        .await
    {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("out/edit2-final.png");
        let _ = std::fs::write(&p, &bytes);
        println!("截图已存 {}", p.display());
    }
    // Escape 兜底关闭
    for t in [
        chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyDown,
        chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyUp,
    ] {
        let _ = page
            .execute(
                chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams::builder()
                    .r#type(t)
                    .key("Escape")
                    .code("Escape")
                    .windows_virtual_key_code(27)
                    .build()
                    .expect("key params"),
            )
            .await;
    }
}


// ── apis：前端 bundle 静态分析——列出全部 CGI 路径与编辑相关上下文 ──
//
// 动机：行内「修改描述和封面」的对话框在自动化上下文打不开（事件到达但无
// 可见响应），人工校准窗口两次错过。bundle 里直接藏著编辑 CGI 的路径与
// 请求体构造代码——只读分析，零交互。

async fn probe_apis(page: &Page, out_dir: &PathBuf) {
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 1) 枚举所有已加载 JS 资源（主文档 + wujie iframe 的都会走网络层）
    let urls_js = r#"(function(){
      return JSON.stringify(performance.getEntriesByType('resource')
        .map(r => r.name)
        .filter(n => n.includes('.js')));
    })()"#;
    let urls: Vec<String> = page
        .evaluate(urls_js)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    println!("已加载 JS 资源 {} 个", urls.len());

    // 2) 逐个 fetch（同源/跨域 CDN 都可读），提取 cgi-bin 路径
    let extract_js = r#"(async (url) => {
      try {
        const t = await (await fetch(url)).text();
        const paths = new Set();
        for (const m of t.matchAll(/\/cgi-bin\/mmfinderassistant-bin\/[A-Za-z0-9_\-\/]+/g)) paths.add(m[0]);
        // 编辑语义关键词上下文
        const ctx = [];
        for (const kw of ['修改描述', 'editFeed', 'modifyFeed', 'edit_desc', 'modify_desc', 'update_desc', 'edit_post', 'update_post', 'shortTitle', 'short_title']) {
          let i = -1;
          while ((i = t.indexOf(kw, i + 1)) >= 0 && ctx.length < 12) {
            ctx.push(kw + ' >>> ' + t.slice(Math.max(0, i - 160), i + 240).replace(/\s+/g, ' '));
          }
        }
        // 关键调用点深挖（更长窗口）：
        //  - finder_update_post_info 的请求体构造（提交 CGI）
        //  - PostCoverEdit 路由路径（编辑页直达 URL）
        //  - finder_author_get_post_verify_info（可改次数/资格查询）
        const deep = [];
        for (const kw of ['finder_update_post_info', 'finderUpdatePostInfo', 'PostCoverEdit', 'finder_author_get_post_verify_info', 'finderAuthorGetPostVerifyInfo', 'post/cover', 'coverEdit', 'cover-edit']) {
          let i = -1, n = 0;
          while ((i = t.indexOf(kw, i + 1)) >= 0 && n < 4) {
            deep.push('### ' + kw + ' >>> ' + t.slice(Math.max(0, i - 700), i + 900).replace(/\s+/g, ' '));
            n++;
          }
        }
        return JSON.stringify({url, size: t.length, paths: [...paths], ctx, deep});
      } catch (e) { return JSON.stringify({url, error: String(e)}); }
    })"#;
    let mut all_paths = std::collections::BTreeSet::new();
    let mut ctx_lines = Vec::new();
    let mut deep_lines = Vec::new();
    for url in &urls {
        let js = format!("({extract_js})({url:?})");
        let s = page
            .evaluate(js)
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default();
        let Ok(j) = serde_json::from_str::<serde_json::Value>(&s) else { continue };
        if let Some(err) = j.get("error") {
            println!("  [fetch 失败] {}: {}", &url[url.len().min(60)..], err);
            continue;
        }
        let paths: Vec<String> = j
            .get("paths")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let ctx: Vec<String> = j
            .get("ctx")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let deep: Vec<String> = j
            .get("deep")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if !paths.is_empty() || !ctx.is_empty() || !deep.is_empty() {
            println!(
                "  {} ({}B): {} 个 CGI 路径, {} 条编辑上下文, {} 条深挖",
                url.rsplit('/').next().unwrap_or(url),
                j.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                paths.len(),
                ctx.len(),
                deep.len()
            );
        }
        for p in paths {
            all_paths.insert(p);
        }
        ctx_lines.extend(ctx);
        deep_lines.extend(deep);
        // 全部 bundle 落盘（broker 基类在无业务命中的 vendor chunk 里；公开静态资源，无凭证）
        {
            let save_js = format!(
                r#"(async (url) => {{ try {{ return await (await fetch(url)).text(); }} catch (e) {{ return ''; }} }})({url:?})"#
            );
            if let Ok(res) = page.evaluate(save_js).await {
                if let Some(text) = res.value().and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        let dir = out_dir.join("js");
                        std::fs::create_dir_all(&dir).expect("mkdir js");
                        let fname = url.rsplit('/').next().unwrap_or("bundle.js");
                        let _ = std::fs::write(dir.join(fname), text);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let log = [
        "== 全部 CGI 路径 ==".to_string(),
        all_paths.iter().cloned().collect::<Vec<_>>().join("\n"),
        "\n== 编辑相关上下文 ==".to_string(),
        ctx_lines.join("\n"),
        "\n== 深挖：提交 CGI / 路由 / 资格查询 ==".to_string(),
        deep_lines.join("\n\n"),
    ]
    .join("\n");
    let log_path = out_dir.join("apis.log");
    std::fs::write(&log_path, &log).expect("write apis log");
    let preview: String = log.chars().take(4000).collect();
    println!("\n{preview}\n结果已存 {}", log_path.display());
}


// ── modfeed：编辑提交 CGI 的只读校准（不修改任何数据）────────────────
//
// bundle 静态分析定案（2026-09-26）：
//   编辑提交 = MmFinderAssistantBroker.finderModFeed(payload)
//   broker 网关 URL = /cgi-bin/mmfinderassistant-bin/svrkit/<Svr>/<method>
//   payload = {exportObjectId, modFlag(位掩码 description:1|coverUrl:2|shortTitle:8),
//              scene:1, description, shortTitle:[{shortTitle}], coverUrl,...}
//   资格查询 = /post/finder_author_get_post_verify_info（可改次数）
// 本探针只做只读验证：资格查询（真 id）+ 假 id 提交探活（绝不碰真视频）。

async fn probe_modfeed(page: &Page) {
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 1) 拿一个真实 objectId（只读）
    let list_env = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":1,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    // body 被 js_fetch 截断（4096），不做完整 JSON 解析，直接抠 objectId 字符串
    let oid = {
        let marker = "\\\"objectId\\\":\\\"";
        list_env
            .find(marker)
            .map(|i| {
                let rest = &list_env[i + marker.len()..];
                rest.split('"')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('\\')
                    .to_string()
            })
            .unwrap_or_default()
    };
    println!("真实 objectId = {oid}");
    if oid.is_empty() {
        println!("拿不到 objectId，探测结束");
        return;
    }

    // 2) 资格查询：finder_author_get_post_verify_info（只读；试两种 id 键名）
    for (key, val) in [("exportObjectId", oid.as_str()), ("objectId", oid.as_str())] {
        let body = format!(r#"{{"{key}":"{val}"}}"#);
        let env = js_fetch(
            page,
            "/cgi-bin/mmfinderassistant-bin/post/finder_author_get_post_verify_info",
            &body,
        )
        .await;
        println!("[verify key={key}] {}", &env[..env.len().min(1200)]);
        tokio::time::sleep(Duration::from_millis(800)).await;
    }

    // 3) 提交 CGI 探活：假 objectId（export/ 前缀 + 全 0 Base64 段），
    //    期望 = 业务错误码（对象不存在类），而非 300004 参数错误/404。
    //    假 id 不可能命中任何真实视频，零副作用。
    //    首次探活返回"接口未授权"——UI 真实请求带 X-WECHAT-UIN 头与
    //    _aid/_rid/_pageUrl 查询参数（middleware 注入），逐个补齐试探。
    let probe_body = r#"{"exportObjectId":"export/UzFfAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","modFlag":"1","scene":1,"description":"probe"}"#;
    let url = "/cgi-bin/mmfinderassistant-bin/svrkit/MmFinderAssistantBroker/finderModFeed";
    // 3a) 裸请求基线（已知：接口未授权）
    let env = js_fetch(page, url, probe_body).await;
    println!("[modfeed 裸] {}", &env[..env.len().min(300)]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // 3b) 带 X-WECHAT-UIN 头
    let js = format!(
        r#"(async () => {{
          try {{
            const r = await fetch({url:?}, {{method:'POST', credentials:'include',
              headers: {{'Content-Type':'application/json', 'X-WECHAT-UIN':'0000000000'}},
              body: {probe_body:?}}});
            return JSON.stringify({{status: r.status, body: (await r.text()).slice(0, 600)}});
          }} catch (e) {{ return 'FETCH_ERR ' + e; }}
        }})()"#
    );
    let env = page.evaluate(js).await.ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    println!("[modfeed +X-WECHAT-UIN] {}", &env[..env.len().min(300)]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // 3c) 头 + 查询参数（_pageUrl 指向 micro/content 子应用路径）
    let url_q = format!(
        "{url}?_aid=00000000-0000-0000-0000-000000000000&_rid=00000000-0000-0000-0000-000000000000&_pageUrl=https%3A%2F%2Fchannels.weixin.qq.com%2Fmicro%2Fcontent%2Fpost%2Flist"
    );
    let js = format!(
        r#"(async () => {{
          try {{
            const r = await fetch({url_q:?}, {{method:'POST', credentials:'include',
              headers: {{'Content-Type':'application/json', 'X-WECHAT-UIN':'0000000000'}},
              body: {probe_body:?}}});
            return JSON.stringify({{status: r.status, body: (await r.text()).slice(0, 600)}});
          }} catch (e) {{ return 'FETCH_ERR ' + e; }}
        }})()"#
    );
    let env = page.evaluate(js).await.ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    println!("[modfeed +头+query] {}", &env[..env.len().min(300)]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // 3d) /micro/content 前缀（wujie 子应用真实网关路径，ACL 可能按前缀放行）
    let micro_url = "/micro/content/cgi-bin/mmfinderassistant-bin/svrkit/MmFinderAssistantBroker/finderModFeed";
    let env = js_fetch(page, micro_url, probe_body).await;
    println!("[modfeed /micro/content 前缀] {}", &env[..env.len().min(300)]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    // 3e) 直接导航到子应用页面（Referer 也变）再试两种路径
    let _ = page
        .goto("https://channels.weixin.qq.com/micro/content/post/list")
        .await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    println!("子应用直达后 href = {}", href(page).await);
    let env = js_fetch(page, micro_url, probe_body).await;
    println!("[modfeed 子应用内 /micro 前缀] {}", &env[..env.len().min(300)]);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let env = js_fetch(page, url, probe_body).await;
    println!("[modfeed 子应用内 裸路径] {}", &env[..env.len().min(300)]);

    // 4) 错误区分矩阵（全假 id，零副作用）：若不同非法输入产生不同错误码，
    //    说明请求已穿透网关到达业务层，/micro/content 前缀即定案。
    let micro = "/micro/content/cgi-bin/mmfinderassistant-bin/svrkit/MmFinderAssistantBroker/finderModFeed";
    let fake = "export/UzFfAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let cases = [
        ("缺 exportObjectId", r#"{"modFlag":"1","scene":1,"description":"probe"}"#.to_string()),
        ("modFlag 非数字", format!(r#"{{"exportObjectId":"{fake}","modFlag":"abc","scene":1,"description":"probe"}}"#)),
        ("modFlag=1 缺 description", format!(r#"{{"exportObjectId":"{fake}","modFlag":"1","scene":1}}"#)),
        ("modFlag=0 空修改", format!(r#"{{"exportObjectId":"{fake}","modFlag":"0","scene":1}}"#)),
        ("完整假 id", format!(r#"{{"exportObjectId":"{fake}","modFlag":"1","scene":1,"description":"probe"}}"#)),
    ];
    for (name, body) in cases {
        let env = js_fetch(page, micro, &body).await;
        println!("[矩阵 {name}] {}", &env[..env.len().min(240)]);
        tokio::time::sleep(Duration::from_millis(600)).await;
    }

    // 5) exportkey：515 框架源码显示它来自页面 URL query（父页面传给 wujie 子应用），
    //    svrkit 网关大概率靠它鉴权。回到外层应用页（/platform/post/list），
    //    从 iframe src / wujie-app 属性里挖出来再试。
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let ek_js = r#"(function(){
      const out = [];
      for (const f of document.querySelectorAll('iframe')) {
        out.push('iframe src=' + (f.src || '').slice(0, 300));
      }
      for (const w of document.querySelectorAll('wujie-app')) {
        out.push('wujie-app attrs: ' + Array.from(w.attributes).map(a => a.name + '=' + String(a.value).slice(0,120)).join(' '));
      }
      // 主页面 URL 上的 exportkey
      out.push('location.search=' + location.search.slice(0, 200));
      return out.join('\n');
    })()"#;
    let dump = page
        .evaluate(ek_js)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    println!("== exportkey 来源排查 ==\n{dump}");
    let exportkey = dump
        .split("exportkey=")
        .nth(1)
        .and_then(|s| s.split(['&', ' ', '"']).next())
        .map(String::from)
        .unwrap_or_default();
    if !exportkey.is_empty() {
        let keyed = format!("{micro}?exportkey={exportkey}");
        let body = format!(r#"{{"exportObjectId":"{fake}","modFlag":"1","scene":1,"description":"probe"}}"#);
        let env = js_fetch(page, &keyed, &body).await;
        println!("[modfeed +exportkey] {}", &env[..env.len().min(300)]);
    } else {
        println!("未找到 exportkey");
    }
}


// ── edit3：直达编辑页（bundle 定案：coverEdit 路由 + ?objectId= 参数）──────
//
// 「修改描述和封面」= Vue 路由 PostCoverEdit（path: coverEdit），页面从
// $route.query.objectId 读目标并调 getPostDetail。子应用可裸开
// （/micro/content/post/list 直达已验证），因此编辑页也可直达：
//   /micro/content/post/coverEdit?objectId=<objectId>
// 本探针只加载页面 + dump DOM/截图，不修改不提交。

async fn probe_edit3(page: &Page) {
    // 拿一个真实 objectId
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let list_env = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":1,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    let marker = "\\\"objectId\\\":\\\"";
    let oid = list_env
        .find(marker)
        .map(|i| {
            let rest = &list_env[i + marker.len()..];
            rest.split('"')
                .next()
                .unwrap_or_default()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_default();
    println!("objectId = {oid}");
    if oid.is_empty() {
        return;
    }

    // 直达编辑页。裸子应用路径 /micro/content/post/coverEdit 返回 Cannot GET
    // （服务端只放 HTML shell，路由在 SPA 内）；改走外层平台路径（wujie 路由同步）。
    for url in [
        format!("https://channels.weixin.qq.com/platform/post/coverEdit?objectId={oid}"),
        format!("https://channels.weixin.qq.com/micro/content/iframe/post-card.html?objectId={oid}#/post/coverEdit"),
    ] {
        let _ = page.goto(&url).await;
        tokio::time::sleep(Duration::from_secs(8)).await;
        println!("尝试 {url}\n  → href = {}", href(page).await);
        let has = page
            .evaluate(
                r#"(function(){
                  const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
                  roots.push(document);
                  for (const r of roots) {
                    for (const el of r.querySelectorAll('[class*=edit-card], [class*=edit-item], [class*=cover-edit]')) {
                      if (el.offsetWidth || el.offsetHeight) return 'found ' + el.className.slice(0, 60);
                    }
                  }
                  return 'none';
                })()"#,
            )
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default();
        println!("  编辑页特征元素: {has}");
        if has.starts_with("found") {
            break;
        }
    }

    if let Ok(bytes) = page
        .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
        .await
    {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("out/edit3-page.png");
        let _ = std::fs::write(&p, &bytes);
        println!("截图已存 {}", p.display());
    }

    // dump 页面控件与结构（编辑页在 wujie shadowRoot 里，须全根遍历）
    let dump_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      const out = [];
      const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
      for (const r of roots) {
        for (const el of r.querySelectorAll('input, textarea, [contenteditable], button, [class*=btn]')) {
          if (!vis(el)) continue;
          const b = el.getBoundingClientRect();
          out.push([
            el.tagName,
            el.type ? 'type=' + el.type : '',
            el.accept ? 'accept=' + el.accept : '',
            el.getAttribute && el.getAttribute('placeholder') ? 'ph=' + el.getAttribute('placeholder') : '',
            el.contentEditable && el.contentEditable !== 'inherit' ? 'ce=' + el.contentEditable : '',
            typeof el.className === 'string' && el.className ? 'class=' + el.className.slice(0, 60) : '',
            el.innerText ? 'text=' + el.innerText.trim().slice(0, 24) : '',
            '@' + Math.round(b.x) + ',' + Math.round(b.y),
          ].filter(Boolean).join(' '));
        }
        // 描述/短标题文本容器
        for (const sel of ['.edit-item', '[class*=desc]', '[class*=short-title]', '[class*=cover']) {
          for (const el of r.querySelectorAll(sel)) {
            if (!vis(el)) continue;
            const t = (el.innerText || '').trim().slice(0, 60).replace(/\n/g, '⏎');
            if (t) out.push('SECTION ' + sel + ' :: ' + t);
          }
        }
      }
      return out.slice(0, 80).join('\n');
    })()"#;
    match page.evaluate(dump_js).await {
        Ok(res) => println!("== 编辑页控件 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("dump 失败: {e}"),
    }
    // 网络观察：getPostDetail 走了哪个端点（仅供 record，抓 5s）
    // （本探针不挂 Network 监听；端点从 bundle 可另查）
}


// ── edit4：编辑页交互全链路校准（不提交）─────────────────────────────
//
// bundle 定案的交互模型（537.9bd4b6c6.js）：
//   描述/短标题 = 划词编辑：选中文字 → 容器收到 selectionEnd 事件 →
//   mp-popover(.edit-pop) 弹出「修改/删除」→ 点修改 → span.editing.edited
//   包裹选区 + .input-item-input 输入框 → 点输入框旁「修改」(handleDescInput)
//   提交编辑动作（计入 editCount，描述总共 20 字预算，短标题 16）。
//   完成 → handleOpenConfirmDialog → 确认弹窗（confirm-preview 预览
//   + 「仅支持修改一次，修改后不可撤回」提示 + 确认修改/取消按钮）。
//   确认修改 → handleConfirm → getEditedText 合并全文 → finderModFeed。
// 本探针走完到确认弹窗为止，最后点「取消」，绝不提交。
// 顺带 dump 封面「编辑」按钮打开的 FinderCoverEditor 结构。

async fn eval_str(page: &Page, js: &str) -> String {
    page.evaluate(js)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default()
}

/// 在可见 weui 对话框内找指定文本按钮的中心坐标。
async fn dialog_button_pos(page: &Page, text: &str) -> Option<(f64, f64)> {
    let js = format!(
        r#"(function(){{
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) {{
        for (const w of r.querySelectorAll('.weui-desktop-dialog__wrp')) {{
          if (!(w.offsetWidth || w.offsetHeight)) continue;
          for (const b of w.querySelectorAll('button')) {{
            if ((b.innerText || '').trim() !== {text:?}) continue;
            const r2 = b.getBoundingClientRect();
            return JSON.stringify({{x: r2.x + r2.width/2, y: r2.y + r2.height/2}});
          }}
        }}
      }}
      return '';
    }})()"#
    );
    let s = eval_str(page, &js).await;
    if s.is_empty() {
        return None;
    }
    let p: serde_json::Value = serde_json::from_str(&s).ok()?;
    Some((
        p.get("x").and_then(|v| v.as_f64())?,
        p.get("y").and_then(|v| v.as_f64())?,
    ))
}

async fn shot(page: &Page, name: &str) {
    if let Ok(bytes) = page
        .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
        .await
    {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("out")
            .join(name);
        let _ = std::fs::write(&p, &bytes);
        println!("截图已存 {}", p.display());
    }
}

async fn probe_edit4(page: &Page) {
    // 0) 拿真实 objectId 并直达编辑页（edit3 已验证路径）
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let list_env = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":1,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    let marker = "\\\"objectId\\\":\\\"";
    let oid = list_env
        .find(marker)
        .map(|i| {
            list_env[i + marker.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_default();
    println!("objectId = {oid}");
    if oid.is_empty() {
        return;
    }
    let _ = page
        .goto(format!(
            "https://channels.weixin.qq.com/platform/post/coverEdit?objectId={oid}"
        ))
        .await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let ready = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          for (const r of roots) {
            const el = r.querySelector('.edit-select-area');
            if (el && (el.offsetWidth || el.offsetHeight)) return 'ready';
          }
          return 'not-ready';
        })()"#,
    )
    .await;
    println!("编辑页就绪 = {ready}, href = {}", href(page).await);
    if ready != "ready" {
        shot(page, "edit4-notready.png").await;
        return;
    }

    // 1) 划词选中描述前 4 个字 → 触发 selectionEnd → 弹层
    let sel = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const r of roots) {
            for (const el of r.querySelectorAll('.edit-select-area')) {
              if (!vis(el)) continue;
              const tn = [...el.childNodes].find(n => n.nodeType === 3 && n.textContent.trim().length >= 4);
              if (!tn) continue;
              const range = document.createRange();
              range.setStart(tn, 0); range.setEnd(tn, 4);
              const s = window.getSelection(); s.removeAllRanges(); s.addRange(range);
              el.dispatchEvent(new Event('selectionEnd'));
              if (el.parentElement) el.parentElement.dispatchEvent(new Event('selectionEnd'));
              return 'selected "' + range.toString() + '" on ' + el.className;
            }
          }
          return 'no-container';
        })()"#,
    )
    .await;
    println!("划词结果: {sel}");
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 2) dump 弹层状态
    let pop = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const el of r.querySelectorAll('.edit-pop, mp-popover, [class*=popover]')) {
              out.push(el.tagName + ' class=' + (typeof el.className === 'string' ? el.className.slice(0,50) : '')
                + ' vis=' + vis(el) + ' text=' + (el.innerText || '').trim().slice(0, 40).replace(/\n/g, '|'));
            }
          }
          return out.slice(0, 20).join('\n') || '(无 popover)';
        })()"#,
    )
    .await;
    println!("== 划词弹层 ==\n{pop}");
    shot(page, "edit4-pop.png").await;

    // 3) 点弹层里的「修改」
    let pos = find_pos(page, ".edit-pop-content div", "修改").await;
    println!("弹层「修改」位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 4) dump 输入框并填入替换文字（handleDescInput 直接读 DOM value，但仍发 input 事件保真）
    let inp = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const el of r.querySelectorAll('.input-item-input, [class*=edit-input] button, [class*=edit-input] input')) {
              if (!vis(el)) continue;
              out.push(el.tagName + ' class=' + (typeof el.className === 'string' ? el.className.slice(0,50) : '')
                + ' text=' + (el.innerText || '').trim().slice(0, 20));
            }
            const inp = r.querySelector('.input-item-input');
            if (inp && vis(inp)) {
              inp.value = '测试改';
              inp.dispatchEvent(new Event('input', {bubbles: true}));
              out.push('VALUE-SET');
            }
            // 编辑态 span 是否已包上
            const ed = r.querySelectorAll('.edit-select-area .editing.edited, .edit-select-area .edited');
            out.push('edited-spans=' + ed.length);
          }
          return out.join('\n') || '(无输入框)';
        })()"#,
    )
    .await;
    println!("== 修改输入框 ==\n{inp}");
    shot(page, "edit4-input.png").await;

    // 5) 点输入框旁「修改」按钮（handleDescInput）提交这条编辑动作
    let pos = find_pos(page, ".edit-input-btns button", "修改").await;
    println!("输入框「修改」按钮位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 6) dump 编辑后状态：edited span、已修改字数徽标、完成按钮可用性
    let st = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            const ed = r.querySelectorAll('.edit-select-area .edited');
            out.push('edited-spans=' + ed.length);
            for (const el of r.querySelectorAll('.edited-count, [class*=count]')) {
              if (vis(el)) out.push('COUNT: ' + (el.innerText || '').trim().slice(0, 40).replace(/\n/g, '|'));
            }
            for (const b of r.querySelectorAll('button')) {
              const t = (b.innerText || '').trim();
              if (t === '完成' || t === '取消') out.push('BTN ' + t + ' disabled=' + b.disabled + ' class=' + (b.className || '').slice(0, 60));
            }
            const desc = r.querySelector('.edit-select-area');
            if (desc) out.push('DESC-HTML: ' + desc.innerHTML.slice(0, 300));
          }
          return out.join('\n');
        })()"#,
    )
    .await;
    println!("== 编辑动作提交后 ==\n{st}");

    // 7) 点「完成」→ dump 确认弹窗
    let pos = find_pos(page, "button", "完成").await;
    println!("「完成」位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let dlg = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const out = [];
          for (const r of roots) {
            for (const w of r.querySelectorAll('.weui-desktop-dialog__wrp')) {
              if (!(w.offsetWidth || w.offsetHeight)) continue;
              out.push('DIALOG: ' + (w.innerText || '').trim().slice(0, 400).replace(/\n/g, ' | '));
              for (const b of w.querySelectorAll('button')) {
                out.push('  DLG-BTN: ' + (b.innerText || '').trim() + ' class=' + (b.className || '').slice(0, 60));
              }
            }
          }
          return out.join('\n') || '(无对话框)';
        })()"#,
    )
    .await;
    println!("== 确认弹窗 ==\n{dlg}");
    shot(page, "edit4-confirm.png").await;

    // 8) 点弹窗里的「取消」——绝不提交
    let pos = dialog_button_pos(page, "取消").await;
    println!("弹窗「取消」位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    // 9) 封面「编辑」按钮 → FinderCoverEditor 结构 dump
    let pos = find_pos(page, ".edit-btn", "编辑").await;
    println!("封面「编辑」位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let cover = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const w of r.querySelectorAll('.weui-desktop-dialog__wrp, [class*=cover-edit], [class*=cover-wrap]')) {
              if (!vis(w)) continue;
              const t = (w.innerText || '').trim().slice(0, 300).replace(/\n/g, ' | ');
              if (t) out.push('COVER-UI ' + (typeof w.className === 'string' ? w.className.slice(0,50) : '') + ': ' + t);
              for (const b of w.querySelectorAll('button')) {
                if (vis(b)) out.push('  BTN: ' + (b.innerText || '').trim().slice(0, 20) + ' class=' + (b.className || '').slice(0, 50));
              }
              for (const f of w.querySelectorAll('input[type=file]')) {
                out.push('  FILE: accept=' + (f.accept || '') + ' class=' + (typeof f.className === 'string' ? f.className.slice(0,50) : ''));
              }
            }
          }
          return out.slice(0, 40).join('\n') || '(无封面编辑器)';
        })()"#,
    )
    .await;
    println!("== 封面编辑器 ==\n{cover}");
    shot(page, "edit4-cover.png").await;
}


// ── edit5：封面编辑器校准（edit4 发现预览图生成中点不开，须先等就绪）───────
//
// FinderCoverEditor（841.d575b04c.js）：步骤流「选择封面 → 调整展示区域」，
// SingleCoverUploader 内有 file input。本探针等预览图就绪后点第一个「编辑」
// （个人主页卡片 3:4），dump 编辑器结构 + file input，然后关闭，不上传不确认。

async fn probe_edit5(page: &Page) {
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let list_env = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":1,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    let marker = "\\\"objectId\\\":\\\"";
    let oid = list_env
        .find(marker)
        .map(|i| {
            list_env[i + marker.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_default();
    println!("objectId = {oid}");
    if oid.is_empty() {
        return;
    }
    let _ = page
        .goto(format!(
            "https://channels.weixin.qq.com/platform/post/coverEdit?objectId={oid}"
        ))
        .await;

    // 等预览图就绪（「预览图生成中，请等待完成后再编辑」消失），最多 120s
    let pop_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) {
        for (const el of r.querySelectorAll('.img-popover-wrap .weui-desktop-popover__desc')) {
          return (el.innerText || '').trim();
        }
      }
      return '';
    })()"#;
    let mut ready = false;
    for i in 0..40 {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let t = eval_str(page, pop_js).await;
        // 就绪 = 编辑按钮已渲染可见 且 预览图不再「生成中」
        let btn_ready = eval_str(
            page,
            r#"(function(){
              const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
              roots.push(document);
              const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
              for (const r of roots) {
                for (const el of r.querySelectorAll('.edit-btn')) {
                  if (vis(el) && (el.innerText || '').trim() === '编辑') return 'y';
                }
              }
              return 'n';
            })()"#,
        )
        .await;
        if i % 5 == 0 || btn_ready == "y" {
            println!(
                "就绪检查[{i}]: 编辑按钮={btn_ready} 预览提示={}",
                if t.is_empty() { "(无)" } else { &t }
            );
        }
        if btn_ready == "y" && (t.is_empty() || !t.contains("生成中")) {
            ready = true;
            break;
        }
    }
    println!("预览就绪 = {ready}");

    // 点第一个「编辑」（个人主页卡片 3:4）
    let pos = find_pos(page, ".edit-btn", "编辑").await;
    println!("封面「编辑」位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // dump 编辑器：可见 dialog/drawer 全文 + 按钮 + file input + 步骤条
    let dump = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const w of r.querySelectorAll('.weui-desktop-dialog__wrp, .weui-desktop-drawer, [class*=drawer], [class*=cover-modal], [class*=cover-set]')) {
              if (!vis(w)) continue;
              const t = (w.innerText || '').trim().slice(0, 500).replace(/\n/g, ' | ');
              if (!t) continue;
              out.push('UI ' + (typeof w.className === 'string' ? w.className.slice(0,60) : w.tagName) + ': ' + t);
              for (const b of w.querySelectorAll('button, [class*=btn]')) {
                if (vis(b)) out.push('  BTN: ' + (b.innerText || '').trim().slice(0, 20) + ' tag=' + b.tagName + ' class=' + (typeof b.className === 'string' ? b.className.slice(0,50) : ''));
              }
              for (const f of w.querySelectorAll('input[type=file]')) {
                out.push('  FILE: accept=' + (f.accept || '') + ' class=' + (typeof f.className === 'string' ? f.className.slice(0,50) : ''));
              }
            }
            // file input 可能在 dialog 外
            for (const f of r.querySelectorAll('input[type=file]')) {
              out.push('FILE(anywhere): accept=' + (f.accept || '') + ' class=' + (typeof f.className === 'string' ? f.className.slice(0,50) : ''));
            }
          }
          return out.slice(0, 60).join('\n') || '(无封面编辑器 UI)';
        })()"#,
    )
    .await;
    println!("== 封面编辑器 ==\n{dump}");
    shot(page, "edit5-cover-editor.png").await;

    // 关闭：优先对话框内「取消」，兜底 Escape
    let pos = dialog_button_pos(page, "取消").await;
    println!("编辑器「取消」位置: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    } else {
        for t in [
            chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyDown,
            chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyUp,
        ] {
            let _ = page
                .execute(
                    chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams::builder()
                        .r#type(t)
                        .key("Escape")
                        .code("Escape")
                        .windows_virtual_key_code(27)
                        .build()
                        .expect("key params"),
                )
                .await;
        }
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    shot(page, "edit5-after-close.png").await;
}


// ── edit7：短标题划词编辑链路诊断（描述链路 edit4 已通，短标题徽标未出）────

async fn probe_edit7(page: &Page) {
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let list_env = js_fetch(
        page,
        "/cgi-bin/mmfinderassistant-bin/post/post_list",
        r#"{"pageSize":1,"currentPage":1,"userpageType":11,"stickyOrder":false}"#,
    )
    .await;
    let marker = "\\\"objectId\\\":\\\"";
    let oid = list_env
        .find(marker)
        .map(|i| {
            list_env[i + marker.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .trim_end_matches('\\')
                .to_string()
        })
        .unwrap_or_default();
    println!("objectId = {oid}");
    if oid.is_empty() {
        return;
    }
    let _ = page
        .goto(format!(
            "https://channels.weixin.qq.com/platform/post/coverEdit?objectId={oid}"
        ))
        .await;
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 选中短标题的「钟表」二字（短标题容器是第二个 .edit-select-area）
    let sel = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const r of roots) {
            for (const el of r.querySelectorAll('.edit-shorttitle-content')) {
              if (!vis(el)) continue;
              const tn = [...el.childNodes].find(n => n.nodeType === 3 && n.textContent.trim().length >= 6);
              if (!tn) return 'no-text-node: ' + el.innerHTML.slice(0, 120);
              const range = document.createRange();
              range.setStart(tn, 4); range.setEnd(tn, 6);
              const s = window.getSelection(); s.removeAllRanges(); s.addRange(range);
              el.dispatchEvent(new Event('selectionEnd'));
              if (el.parentElement) el.parentElement.dispatchEvent(new Event('selectionEnd'));
              return 'selected "' + range.toString() + '" of "' + tn.textContent + '"';
            }
          }
          return 'no-container';
        })()"#,
    )
    .await;
    println!("划词: {sel}");
    tokio::time::sleep(Duration::from_millis(800)).await;
    {
        let s = eval_str(page, r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const el of r.querySelectorAll('.edit-pop .weui-desktop-popover')) {
              const b = el.getBoundingClientRect();
              out.push('pop vis=' + vis(el) + ' text=' + (el.innerText||'').trim().slice(0,30).replace(/\n/g,'|') + ' @' + Math.round(b.x) + ',' + Math.round(b.y));
            }
          }
          return out.join('\n') || '(无弹层)';
        })()"#).await;
        println!("== 弹层状态 ==\n{s}");
    }

    // 点弹层「修改」
    let pos = find_pos(page, ".edit-pop-content div", "修改").await;
    println!("弹层「修改」: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    {
        let s = eval_str(page, r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const el of r.querySelectorAll('.input-item-input, .edit-input-btns button, .edit-select-area .editing, .weui-desktop-tips')) {
              out.push(el.tagName + '.' + (typeof el.className === 'string' ? el.className.slice(0,40) : '') + ' vis=' + vis(el) + ' text=' + (el.innerText || '').trim().slice(0,40));
            }
          }
          return out.join('\n') || '(空)';
        })()"#).await;
        println!("== 点修改后 ==\n{s}");
    }

    // 填值 + 提交编辑动作
    let fill = eval_str(
        page,
        r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const r of roots) {
            for (const inp of r.querySelectorAll('.input-item-input')) {
              if (!vis(inp)) continue;
              inp.value = '时钟';
              inp.dispatchEvent(new Event('input', {bubbles: true}));
              return 'set';
            }
          }
          return 'no-input';
        })()"#,
    )
    .await;
    println!("填值: {fill}");
    let pos = find_pos(page, ".edit-input-btns button", "修改").await;
    println!("输入框「修改」: {pos:?}");
    if let Some((x, y)) = pos {
        cdp_click(page, x, y).await;
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    {
        let s = eval_str(page, r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const r of roots) {
            for (const el of r.querySelectorAll('.edited-count, .edit-select-area .edited, .weui-desktop-tips, .edit-shorttitle-content')) {
              out.push((typeof el.className === 'string' ? el.className.slice(0,45) : el.tagName) + ' vis=' + vis(el)
                + ' text=' + (el.innerText || '').trim().slice(0, 60).replace(/\n/g, '|'));
            }
          }
          return out.join('\n') || '(空)';
        })()"#).await;
        println!("== 提交编辑动作后 ==\n{s}");
    }
    shot(page, "edit7-final.png").await;
}


// ── edit：自动进视频管理页、点开「修改描述和封面」，dump 表单（不提交）──────
//
// 自动点击（scrollIntoView + hover + CDP 真鼠标 + 合成 click 兜底）失败时，
// 等人工点一次（轮询可见对话框 150s）。全程抓 CGI 流量（过滤心跳/上报噪声），
// 打开对话框触发的详情请求也会被记录。

const DIALOG_OPEN_JS: &str = r#"(function(){
  const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
  roots.push(document);
  for (const r of roots) {
    for (const el of r.querySelectorAll('.weui-desktop-dialog__wrp')) {
      if (el.offsetWidth || el.offsetHeight) return 'open';
    }
  }
  return 'closed';
})()"#;

async fn dump_edit_form(page: &Page) {
    // 表单控件
    let form_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
      const out = [];
      for (const r of roots) {
        for (const el of r.querySelectorAll('input, textarea, [contenteditable=true], [contenteditable=""], button')) {
          if (!vis(el)) continue;
          const desc = [
            el.tagName,
            el.type ? 'type=' + el.type : '',
            el.accept ? 'accept=' + el.accept : '',
            el.getAttribute && el.getAttribute('placeholder') ? 'ph=' + el.getAttribute('placeholder') : '',
            el.contentEditable && el.contentEditable !== 'inherit' ? 'ce=' + el.contentEditable : '',
            typeof el.className === 'string' && el.className ? 'class=' + el.className.slice(0, 80) : '',
            el.innerText ? 'text=' + el.innerText.trim().slice(0, 30) : '',
          ].filter(Boolean).join(' ');
          out.push(desc);
        }
      }
      return out.slice(0, 80).join('\n');
    })()"#;
    match page.evaluate(form_js).await {
        Ok(res) => println!("== 编辑表单控件 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("dump 表单失败: {e}"),
    }

    // 可见对话框的 outerHTML（不假设 class 名）
    let html_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) {
        for (const w of r.querySelectorAll('.weui-desktop-dialog__wrp')) {
          if (!(w.offsetWidth || w.offsetHeight)) continue;
          let host = w.closest('[class*=dialog]') || w;
          return host.outerHTML.slice(0, 8000);
        }
      }
      return '(无可见对话框)';
    })()"#;
    match page.evaluate(html_js).await {
        Ok(res) => println!("== 可见对话框 outerHTML ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("dump outerHTML 失败: {e}"),
    }

    // file input（不管可见性）
    let misc_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      const out = [];
      for (const r of roots) {
        for (const el of r.querySelectorAll('input[type=file]')) {
          out.push('FILE accept=' + (el.accept || '') + ' class=' + (typeof el.className === 'string' ? el.className : ''));
        }
      }
      return out.join('\n') || '(无 file input)';
    })()"#;
    match page.evaluate(misc_js).await {
        Ok(res) => println!("== file inputs ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("dump misc 失败: {e}"),
    }

    if let Ok(bytes) = page
        .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
        .await
    {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("out/edit-dialog.png");
        let _ = std::fs::write(&p, &bytes);
        println!("截图已存 {}", p.display());
    }
}

async fn probe_edit(page: &Page, out_dir: &PathBuf) {
    // 抓 CGI（过滤 helper/report 噪声），记录打开编辑对话框触发的请求
    page.execute(EnableParams::default()).await.expect("network enable");
    let mut requests = page
        .event_listener::<EventRequestWillBeSent>()
        .await
        .expect("request listener");
    let log_path = out_dir.join(format!(
        "edit-sniff-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log2 = log.clone();
    let page2 = page.clone();
    let net = tokio::spawn(async move {
        while let Some(ev) = requests.next().await {
            let url = ev.request.url.clone();
            if !(url.contains("cgi-bin") || url.contains("mmfinder")) {
                continue;
            }
            if url.contains("helper_") || url.contains("hepler_") || url.contains("report-perf")
                || url.contains("heartbeat") || url.contains("auth_110") {
                continue;
            }
            let mut line = format!("[REQ] {} {}", ev.request.method, url);
            if ev.request.has_post_data == Some(true) {
                if let Ok(pd) = page2
                    .execute(GetRequestPostDataParams::new(ev.request_id.clone()))
                    .await
                {
                    let trimmed: String = pd.post_data.chars().take(400).collect();
                    line.push_str(&format!(" post_data={trimmed}"));
                }
            }
            println!("{line}");
            log2.lock().unwrap().push(line);
        }
    });

    // route 探针已确认直达 post/list 不会被弹回主页
    let _ = page.goto(POST_LIST_URL).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    println!("当前 href = {}", href(page).await);

    // 0) 诊断：目标坐标处 elementFromPoint 命中的到底是什么（排查透明遮罩拦截）
    let diag_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) {
        for (const el of r.querySelectorAll('.edit-cover-text')) {
          el.scrollIntoView({block: 'center', inline: 'center'});
          const b = el.getBoundingClientRect();
          const x = b.x + b.width/2, y = b.y + b.height/2;
          // 在 shadowRoot 上下文里 elementFromPoint 才能穿透影子树
          const hit = r.elementFromPoint ? r.elementFromPoint(x, y) : null;
          const chain = [];
          let cur = hit;
          while (cur && chain.length < 6) {
            chain.push(cur.tagName + '.' + (typeof cur.className === 'string' ? cur.className.slice(0,60) : ''));
            cur = cur.parentElement || (cur.getRootNode && cur.getRootNode().host);
          }
          return JSON.stringify({x, y, hit: chain, targetRect: {x: b.x, y: b.y, w: b.width, h: b.height}});
        }
      }
      return '(no entry)';
    })()"#;
    match page.evaluate(diag_js).await {
        Ok(res) => println!("== elementFromPoint 诊断 ==\n{}", res.value().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()),
        Err(e) => println!("诊断失败: {e}"),
    }

    // 0b) 对照实验：点「评论管理」（同一条 hover 操作条的另一项），看点击机制是否整体有效
    let ctrl_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) {
        for (const el of r.querySelectorAll('.action-content')) {
          if ((el.innerText || '').trim() !== '评论管理') continue;
          el.scrollIntoView({block: 'center', inline: 'center'});
          const b = el.getBoundingClientRect();
          return JSON.stringify({x: b.x + b.width/2, y: b.y + b.height/2});
        }
      }
      return '';
    })()"#;
    let ctrl = page
        .evaluate(ctrl_js)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    if !ctrl.is_empty() {
        let p: serde_json::Value = serde_json::from_str(&ctrl).expect("ctrl json");
        let (cx, cy) = (
            p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
        };
        let mv = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(cx)
            .y(cy)
            .build()
            .expect("mv");
        let _ = page.execute(mv).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        for t in [DispatchMouseEventType::MousePressed, DispatchMouseEventType::MouseReleased] {
            let params = DispatchMouseEventParams::builder()
                .r#type(t)
                .x(cx)
                .y(cy)
                .button(MouseButton::Left)
                .click_count(1)
                .build()
                .expect("mouse params");
            let _ = page.execute(params).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let st = page
            .evaluate(DIALOG_OPEN_JS)
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default();
        println!("对照实验（点评论管理）后对话框状态: {st}, href = {}", href(page).await);
        if let Ok(bytes) = page
            .screenshot(chromiumoxide::page::ScreenshotParams::builder().build())
            .await
        {
            let pth = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("out/control-comment.png");
            let _ = std::fs::write(&pth, &bytes);
            println!("对照截图已存 {}", pth.display());
        }
        // 关掉对照面板，回到列表
        for t in [
            chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyDown,
            chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyUp,
        ] {
            let _ = page
                .execute(
                    chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams::builder()
                        .r#type(t)
                        .key("Escape")
                        .code("Escape")
                        .windows_virtual_key_code(27)
                        .build()
                        .expect("key params"),
                )
                .await;
        }
        let _ = page.goto(POST_LIST_URL).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // 自动尝试：scrollIntoView 第一行的「修改描述和封面」→ hover → CDP 点击 → 合成 click 兜底
    let entry_js = r#"(function(){
      const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
      roots.push(document);
      for (const r of roots) {
        for (const el of r.querySelectorAll('.edit-cover-text')) {
          el.scrollIntoView({block: 'center', inline: 'center'});
          const b = el.getBoundingClientRect();
          return JSON.stringify({x: b.x + b.width/2, y: b.y + b.height/2});
        }
      }
      return '';
    })()"#;
    let pos = page
        .evaluate(entry_js)
        .await
        .ok()
        .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();
    if !pos.is_empty() {
        let p: serde_json::Value = serde_json::from_str(&pos).expect("pos json");
        let (x, y) = (
            p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
        );
        let mv = chromiumoxide::cdp::browser_protocol::input::DispatchMouseEventParams::builder()
            .r#type(chromiumoxide::cdp::browser_protocol::input::DispatchMouseEventType::MouseMoved)
            .x(x)
            .y(y)
            .build()
            .expect("mouse move");
        let _ = page.execute(mv).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        for t in [
            chromiumoxide::cdp::browser_protocol::input::DispatchMouseEventType::MousePressed,
            chromiumoxide::cdp::browser_protocol::input::DispatchMouseEventType::MouseReleased,
        ] {
            let params = chromiumoxide::cdp::browser_protocol::input::DispatchMouseEventParams::builder()
                .r#type(t)
                .x(x)
                .y(y)
                .button(chromiumoxide::cdp::browser_protocol::input::MouseButton::Left)
                .click_count(1)
                .build()
                .expect("mouse params");
            let _ = page.execute(params).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = page
            .evaluate(
                r#"(function(){
                  const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
                  roots.push(document);
                  for (const r of roots) {
                    const el = r.querySelector('.edit-cover-item');
                    if (el) { el.click(); return true; }
                  }
                  return false;
                })()"#,
            )
            .await;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    // 自动路径未开对话框 → 等人工点一次（不要提交！）
    let mut opened = String::new();
    for _ in 0..3 {
        opened = page
            .evaluate(DIALOG_OPEN_JS)
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default();
        if opened == "open" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if opened != "open" {
        println!("**********************************************************");
        println!("自动点击未打开编辑对话框。请在浏览器里手动点一次任意视频");
        println!("的「修改描述和封面」—— 只打开，千万别点提交/确定！");
        println!("检测到对话框后自动 dump（最多等 600s）。");
        println!("**********************************************************");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            opened = page
                .evaluate(DIALOG_OPEN_JS)
                .await
                .ok()
                .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
                .unwrap_or_default();
            if opened == "open" || tokio::time::Instant::now() >= deadline {
                break;
            }
        }
    }
    println!("对话框状态: {opened}");
    if opened == "open" {
        tokio::time::sleep(Duration::from_secs(2)).await;
        dump_edit_form(page).await;
    }

    // Escape 关闭对话框，绝不提交
    for t in [
        chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyDown,
        chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventType::KeyUp,
    ] {
        let _ = page
            .execute(
                chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams::builder()
                    .r#type(t)
                    .key("Escape")
                    .code("Escape")
                    .windows_virtual_key_code(27)
                    .build()
                    .expect("key params"),
            )
            .await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    net.abort();
    std::fs::write(&log_path, log.lock().unwrap().join("\n")).expect("write edit sniff log");
    println!("抓包日志写到 {}", log_path.display());
    println!("href = {}", href(page).await);
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let probe = args.next().unwrap_or_else(|| "route".into());
    let profile = args.next().map(PathBuf::from).unwrap_or_else(default_profile);
    let chrome = args.next().map(PathBuf::from).unwrap_or_else(default_chrome);

    if !profile.exists() {
        eprintln!("profile 不存在: {}（先 sph login）", profile.display());
        std::process::exit(1);
    }
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");
    std::fs::create_dir_all(&out_dir).expect("mkdir out");

    println!("probe={probe} profile={} chrome={}", profile.display(), chrome.display());
    let (mut browser, page) = launch(&chrome, &profile).await;

    match probe.as_str() {
        "route" => probe_route(&page).await,
        "sniff" => probe_sniff(&page, &out_dir).await,
        "fetch" => probe_fetch(&page).await,
        "collection" => probe_collection(&page).await,
        "clickdbg" => probe_clickdbg(&page).await,
        "edit2" => probe_edit2(&page).await,
        "apis" => probe_apis(&page, &out_dir).await,
        "modfeed" => probe_modfeed(&page).await,
        "edit3" => probe_edit3(&page).await,
        "edit4" => probe_edit4(&page).await,
        "edit5" => probe_edit5(&page).await,
        "edit6" => probe_edit6(&page).await,
        "edit7" => probe_edit7(&page).await,
        "edit" => probe_edit(&page, &out_dir).await,
        other => {
            eprintln!("未知 probe: {other}（route / sniff / fetch / collection / clickdbg / edit2 / apis / modfeed / edit3 / edit4 / edit5 / edit6 / edit）");
            std::process::exit(2);
        }
    }

    let _ = browser.close().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
}
