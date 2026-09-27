//! 视频号助手内部 CGI 的页面上下文拉取与宽容解析。
//!
//! 调用方式：在已登录主页的 JS 上下文里 fetch 站内 CGI（cookie 自动携带），
//! 返回 JSON 字符串到 Rust 侧二次解析。端点与请求体模板可由
//! ~/.sph/patches/videos.json 的 `api` 节覆盖（未文档化接口，漂移可热修）。
//!
//! 解析策略：宽容提取——在响应树里找"元素带 id 字段的对象数组"作为视频列表，
//! 字段按候选键名逐个尝试；只有 id/title 是硬要求。形状不认识时响亮
//! SchemaChanged，绝不静默返回空表。

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::wujie::Wujie;
use crate::upstream::sanitize_message;
use crate::videos::patches::ApiConfig;

/// 单页响应体大小上限（防止异常大响应撑爆 evaluate 通道）。
const BODY_CAP: usize = 256 << 10;
/// 分页之间的间隔（防风控）。
const PAGE_INTERVAL: Duration = Duration::from_millis(200);
/// 分页安全上限（页）。
const MAX_PAGES: u32 = 50;

/// 一条已发布视频。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VideoEntry {
    /// edit 定位 id（objectId，形如 "export/UzFf..."；备选 exportId/feedId）。
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// RFC3339（平台时间戳转换；无法识别则为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub like_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_count: Option<i64>,
}

/// 一页拉取结果。
#[derive(Debug)]
pub struct RawPage {
    pub entries: Vec<VideoEntry>,
    pub has_more: bool,
}

/// 合集元信息（get_collection_list 响应项）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CollectionInfo {
    /// 合集 id（"event/UzFf..."）。
    pub id: String,
    pub name: String,
    pub feed_count: i64,
}

/// 页面上下文 POST 站内 CGI，返回响应体文本。
/// HTTP 状态映射：2xx 放行（平台成功实测为 201）；401/403 → SessionExpired。
async fn post_cgi(
    w: &Wujie<'_>,
    endpoint: &str,
    body: &str,
    timeout: Duration,
    stage: Stage,
) -> Result<String> {
    let js = format!(
        r#"(async () => {{
          try {{
            const r = await fetch({endpoint:?}, {{
              method: 'POST',
              headers: {{'Content-Type': 'application/json'}},
              credentials: 'include',
              body: {body:?}
            }});
            const t = await r.text();
            return JSON.stringify({{status: r.status, body: t.slice(0, {BODY_CAP})}});
          }} catch (e) {{ return JSON.stringify({{status: -1, body: String(e)}}); }}
        }})()"#,
    );
    let fut = w.run_js(&js, stage);
    let raw = match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r?,
        Err(_) => return Err(AppError::new(Code::Timeout, stage, "拉取视频数据超时")),
    };
    let raw =
        raw.ok_or_else(|| AppError::new(Code::SchemaChanged, stage, "CGI 未返回可解析的内容"))?;
    let envelope: Value = serde_json::from_str(&raw)
        .map_err(|_| AppError::new(Code::SchemaChanged, stage, "CGI 返回不是 JSON 信封"))?;
    let status = envelope
        .get("status")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let body_text = envelope
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match status {
        // 平台成功响应实测为 HTTP 201（spike 2026-09-26），接受一切 2xx
        200..=299 => Ok(body_text.to_string()),
        401 | 403 => Err(AppError::new(
            Code::SessionExpired,
            stage,
            "视频号助手会话已失效，请执行 sph login 重新登录。",
        )),
        -1 => Err(AppError::fmt(
            Code::NetworkError,
            stage,
            format_args!("CGI 请求失败: {}", sanitize_message(body_text)),
        )),
        other => Err(AppError::fmt(
            Code::UpstreamError,
            stage,
            format_args!("CGI HTTP {other}: {}", sanitize_message(body_text)),
        )),
    }
}

/// 解析 JSON 根并检查 errcode 语义（0 = 成功；会话类 → SessionExpired；其余响亮）。
fn parse_root(body: &str, stage: Stage) -> Result<Value> {
    let root: Value = serde_json::from_str(body).map_err(|_| {
        AppError::fmt(
            Code::SchemaChanged,
            stage,
            format_args!("CGI 响应不是 JSON: {}", sanitize_message(body)),
        )
    })?;
    if let Some(code) = find_i64(&root, &["errCode", "errcode", "ret", "base_resp.ret"]) {
        if code != 0 {
            let msg = find_string(&root, &["errMsg", "errmsg", "message", "base_resp.errmsg"])
                .unwrap_or_default();
            let lower = msg.to_lowercase();
            if lower.contains("login") || msg.contains("登录") || code == -2002 || code == 41001 {
                return Err(AppError::new(
                    Code::SessionExpired,
                    stage,
                    "视频号助手会话已失效，请执行 sph login 重新登录。",
                ));
            }
            return Err(AppError::fmt(
                Code::UpstreamError,
                stage,
                format_args!("CGI 错误 errcode={code}: {}", sanitize_message(&msg)),
            ));
        }
    }
    Ok(root)
}

/// 请求体模板渲染（__PAGE__ / __PAGE_SIZE__，可选 __COLLECTION_ID__）。
fn render_body(template: &str, page: u32, page_size: u32, collection_id: Option<&str>) -> String {
    let mut s = template
        .replace("__PAGE__", &page.to_string())
        .replace("__PAGE_SIZE__", &page_size.to_string());
    if let Some(cid) = collection_id {
        s = s.replace("__COLLECTION_ID__", cid);
    }
    s
}

/// 拉取一页已发布视频。
pub async fn fetch_page(
    w: &Wujie<'_>,
    api: &ApiConfig,
    page_no: u32,
    timeout: Duration,
) -> Result<RawPage> {
    let body = render_body(&api.post_list_body, page_no, api.page_size, None);
    let body_text = post_cgi(w, &api.post_list_endpoint, &body, timeout, Stage::FetchList).await?;
    parse_page(&body_text, api.page_size)
}

/// 分页拉取直到攒够 limit 或翻完。
pub async fn fetch_all(
    w: &Wujie<'_>,
    api: &ApiConfig,
    limit: usize,
    per_request_timeout: Duration,
) -> Result<Vec<VideoEntry>> {
    let mut out: Vec<VideoEntry> = Vec::new();
    for page_no in 1..=MAX_PAGES {
        let page = fetch_page(w, api, page_no, per_request_timeout).await?;
        let empty = page.entries.is_empty();
        let has_more = page.has_more;
        out.extend(page.entries);
        if empty || !has_more || out.len() >= limit {
            break;
        }
        tokio::time::sleep(PAGE_INTERVAL).await;
    }
    out.truncate(limit);
    Ok(out)
}

/// 拉取全部合集（名称 → id 解析的数据源）。
pub async fn fetch_collections(
    w: &Wujie<'_>,
    api: &ApiConfig,
    per_request_timeout: Duration,
) -> Result<Vec<CollectionInfo>> {
    let mut out: Vec<CollectionInfo> = Vec::new();
    for page_no in 1..=MAX_PAGES {
        let body = render_body(&api.collection_list_body, page_no, api.page_size, None);
        let text = post_cgi(
            w,
            &api.collection_list_endpoint,
            &body,
            per_request_timeout,
            Stage::FetchList,
        )
        .await?;
        let (page, total) = parse_collections_body(&text)?;
        let empty = page.is_empty();
        out.extend(page);
        if empty || out.len() >= total.max(0) as usize {
            break;
        }
        tokio::time::sleep(PAGE_INTERVAL).await;
    }
    Ok(out)
}

/// 拉取某合集内的视频清单（成员关系唯一可靠来源，spike 2026-09-26 定案：
/// post_list 的 collectionId 参数被平台静默忽略）。
pub async fn fetch_collection_feeds(
    w: &Wujie<'_>,
    api: &ApiConfig,
    collection_id: &str,
    limit: usize,
    per_request_timeout: Duration,
) -> Result<Vec<VideoEntry>> {
    let mut out: Vec<VideoEntry> = Vec::new();
    for page_no in 1..=MAX_PAGES {
        let body = render_body(
            &api.collection_feed_body,
            page_no,
            api.page_size,
            Some(collection_id),
        );
        let text = post_cgi(
            w,
            &api.collection_feed_endpoint,
            &body,
            per_request_timeout,
            Stage::FetchList,
        )
        .await?;
        let (page, total) = parse_collection_feed_body(&text)?;
        let empty = page.is_empty();
        out.extend(page);
        if empty || out.len() >= total.max(0) as usize || out.len() >= limit {
            break;
        }
        tokio::time::sleep(PAGE_INTERVAL).await;
    }
    out.truncate(limit);
    Ok(out)
}

/// 解析合集清单响应：data.collectionList[] = {id, name, feedCount}，total=collectionListCount。
fn parse_collections_body(body: &str) -> Result<(Vec<CollectionInfo>, i64)> {
    let root = parse_root(body, Stage::FetchList)?;
    let data = root.get("data").unwrap_or(&root);
    let list = find_string_keyed_array(data, &["collectionList", "collection_list", "list"])
        .ok_or_else(|| {
            AppError::fmt(
                Code::SchemaChanged,
                Stage::FetchList,
                format_args!(
                    "合集清单响应中找不到 collectionList（接口结构可能已变化）: {}",
                    sanitize_message(body)
                ),
            )
        })?;
    let collections: Vec<CollectionInfo> = list
        .iter()
        .filter_map(|c| {
            let id = find_string(c, &["id", "collectionId", "collection_id"])?;
            let name = find_string(c, &["name", "title"])?;
            Some(CollectionInfo {
                id,
                name,
                feed_count: find_i64(c, &["feedCount", "feed_count"]).unwrap_or(-1),
            })
        })
        .collect();
    let total = find_i64(data, &["collectionListCount", "totalCount", "total"])
        .unwrap_or(collections.len() as i64);
    Ok((collections, total))
}

/// 解析合集成员响应：data.objectInfo[] = {feed: <post_list 同形 item>, sortId}，
/// total=collectionFeedCount（实测无 continueFlag，用 total 判定终止）。
fn parse_collection_feed_body(body: &str) -> Result<(Vec<VideoEntry>, i64)> {
    let root = parse_root(body, Stage::FetchList)?;
    let data = root.get("data").unwrap_or(&root);
    let list =
        find_string_keyed_array(data, &["objectInfo", "object_info", "list"]).ok_or_else(|| {
            AppError::fmt(
                Code::SchemaChanged,
                Stage::FetchList,
                format_args!(
                    "合集成员响应中找不到 objectInfo（接口结构可能已变化）: {}",
                    sanitize_message(body)
                ),
            )
        })?;
    let entries: Vec<VideoEntry> = list
        .iter()
        .filter_map(|oi| {
            let feed = oi.get("feed").unwrap_or(oi);
            parse_entry(feed)
        })
        .collect();
    if entries.is_empty() && !list.is_empty() {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::FetchList,
            "合集成员数组存在但元素解析不出 feed.id（接口结构可能已变化）",
        ));
    }
    let total = find_i64(data, &["collectionFeedCount", "totalCount", "total"])
        .unwrap_or(entries.len() as i64);
    Ok((entries, total))
}

/// 按键名候选找对象数组（不做元素形状校验——调用方自行解析）。
fn find_string_keyed_array<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    for key in keys {
        if let Some(Value::Array(arr)) = lookup(v, key) {
            return Some(arr);
        }
    }
    None
}

/// 解析一页响应体（纯函数，fixture 可测）。
fn parse_page(body: &str, page_size: u32) -> Result<RawPage> {
    let root = parse_root(body, Stage::FetchList)?;
    let list = find_entry_array(&root).ok_or_else(|| {
        AppError::fmt(
            Code::SchemaChanged,
            Stage::FetchList,
            format_args!(
                "列表响应中找不到视频数组（接口结构可能已变化）: {}",
                sanitize_message(body)
            ),
        )
    })?;
    let entries: Vec<VideoEntry> = list.iter().filter_map(parse_entry).collect();
    if entries.is_empty() && !list.is_empty() {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::FetchList,
            "列表数组存在但元素解析不出 id/title（接口结构可能已变化）",
        ));
    }
    let has_more_keys = &[
        "hasMore",
        "has_more",
        "hasNextPage",
        "continueFlag",
        "continue_flag",
    ];
    let has_more = find_bool(&root, has_more_keys)
        .or_else(|| root.get("data").and_then(|d| find_bool(d, has_more_keys)))
        .unwrap_or(entries.len() >= page_size as usize && !entries.is_empty());
    Ok(RawPage { entries, has_more })
}

/// 在响应树里找视频数组：优先键名含 list 的对象数组，元素须带 id 候选字段。
fn find_entry_array(root: &Value) -> Option<&Vec<Value>> {
    let mut stack: Vec<&Value> = vec![root];
    // 先搜 data 子树（响应载荷惯例），再全树
    if let Some(data) = root.get("data") {
        stack.insert(0, data);
    }
    let mut visited = 0;
    while let Some(v) = stack.pop() {
        visited += 1;
        if visited > 64 {
            break;
        }
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    if k.to_lowercase().contains("list") {
                        if let Value::Array(arr) = val {
                            if arr
                                .first()
                                .map(|e| extract_id(e).is_some())
                                .unwrap_or(false)
                            {
                                return Some(arr);
                            }
                        }
                    }
                    if matches!(val, Value::Object(_) | Value::Array(_)) {
                        stack.push(val);
                    }
                }
            }
            Value::Array(arr) => {
                if arr
                    .first()
                    .map(|e| extract_id(e).is_some())
                    .unwrap_or(false)
                {
                    return Some(arr);
                }
                for item in arr {
                    if matches!(item, Value::Object(_)) {
                        stack.push(item);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_entry(v: &Value) -> Option<VideoEntry> {
    let id = extract_id(v)?;
    // 实测结构（2026-09-26 spike）：标题/描述在嵌套的 desc 对象里
    //（desc.shortTitle 字符串，或数组形态 desc.shortTitle[0].shortTitle；
    // desc.description），封面在 desc.media[0].coverUrl。
    let desc = v.get("desc");
    let title = find_string(v, &["title", "shortTitle", "short_title"])
        .or_else(|| desc.and_then(|d| find_string(d, &["shortTitle", "short_title", "title"])))
        .or_else(|| {
            // 真实 post_list 的短标题是数组形态：desc.shortTitle[0].shortTitle
            desc.and_then(|d| d.get("shortTitle"))
                .and_then(|s| s.as_array())
                .and_then(|a| a.first())
                .and_then(|o| find_string(o, &["shortTitle", "title", "name"]))
        })
        .or_else(|| {
            desc.and_then(|d| find_string(d, &["description", "desc"]))
                .map(|s| s.chars().take(50).collect())
        })
        .unwrap_or_default();
    let description = find_string(v, &["description"])
        .or_else(|| desc.and_then(|d| find_string(d, &["description", "desc"])));
    let cover_url = find_string(v, &["coverUrl", "coverURL", "cover", "picUrl", "pic_url"])
        .filter(|s| s.starts_with("http"))
        .or_else(|| {
            desc.and_then(|d| d.get("media"))
                .and_then(|m| m.as_array())
                .and_then(|a| a.first())
                .and_then(|m| find_string(m, &["coverUrl", "thumbUrl", "thumb_url"]))
                .filter(|s| s.starts_with("http"))
        });
    let collection = find_string(v, &["collectionName", "collection_name"]).or_else(|| {
        v.get("collectionInfo")
            .or_else(|| v.get("collection"))
            .and_then(|c| find_string(c, &["name", "title"]))
    });
    let created_at = find_i64(
        v,
        &[
            "createTime",
            "createtime",
            "create_time",
            "createTimeMs",
            "create_time_ms",
        ],
    )
    .and_then(format_epoch);
    Some(VideoEntry {
        id,
        title,
        description,
        cover_url,
        collection,
        created_at,
        read_count: find_i64(v, &["readCount", "read_count"]),
        like_count: find_i64(v, &["likeCount", "like_count"]),
        comment_count: find_i64(v, &["commentCount", "comment_count"]),
    })
}

/// id 候选字段：objectId / feedId / exportId / id（字符串或数字）。
fn extract_id(v: &Value) -> Option<String> {
    for key in [
        "objectId",
        "object_id",
        "feedId",
        "feed_id",
        "exportId",
        "export_id",
        "id",
    ] {
        match v.get(key) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(Value::Number(n)) => return Some(n.to_string()),
            _ => {}
        }
    }
    None
}

fn find_string(v: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = lookup(v, key) {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn find_i64(v: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(val) = lookup(v, key) {
            match val {
                Value::Number(n) => return n.as_i64(),
                Value::String(s) => {
                    if let Ok(n) = s.parse::<i64>() {
                        return Some(n);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn find_bool(v: &Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(val) = lookup(v, key) {
            match val {
                Value::Bool(b) => return Some(*b),
                Value::Number(n) => return n.as_i64().map(|x| x != 0),
                _ => {}
            }
        }
    }
    None
}

/// 支持 "a.b" 点路径的对象查找。
fn lookup<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

/// epoch 秒/毫秒 → RFC3339。
fn format_epoch(ts: i64) -> Option<String> {
    let secs = if ts > 1_000_000_000_000 {
        ts / 1000
    } else {
        ts
    };
    if secs <= 0 {
        return None;
    }
    let t = time::OffsetDateTime::from_unix_timestamp(secs).ok()?;
    t.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_shape() {
        let body = r#"{"errCode":0,"data":{"hasMore":true,"list":[
            {"objectId":"abc123","title":"视频一","description":"描述一","coverUrl":"https://cdn/x.jpg","createTime":1790000000,"collectionInfo":{"name":"合集A"}},
            {"objectId":456,"shortTitle":"视频二"}
        ]}}"#;
        let page = parse_page(body, 20).unwrap();
        assert!(page.has_more);
        assert_eq!(page.entries.len(), 2);
        let e = &page.entries[0];
        assert_eq!(e.id, "abc123");
        assert_eq!(e.title, "视频一");
        assert_eq!(e.description.as_deref(), Some("描述一"));
        assert_eq!(e.cover_url.as_deref(), Some("https://cdn/x.jpg"));
        assert_eq!(e.collection.as_deref(), Some("合集A"));
        assert!(e.created_at.as_deref().unwrap().starts_with("2026-"));
        assert_eq!(page.entries[1].id, "456");
        assert_eq!(page.entries[1].title, "视频二");
    }

    #[test]
    fn session_errcode_maps_to_session_expired() {
        let body = r#"{"errCode":-2002,"errMsg":"login expired"}"#;
        let err = parse_page(body, 20).unwrap_err();
        assert_eq!(err.code, Code::SessionExpired);
    }

    #[test]
    fn other_errcode_is_upstream_error() {
        let body = r#"{"errCode":50001,"errMsg":"频率超限"}"#;
        let err = parse_page(body, 20).unwrap_err();
        assert_eq!(err.code, Code::UpstreamError);
        assert!(err.message.contains("频率超限"));
    }

    #[test]
    fn missing_list_is_schema_changed() {
        let body = r#"{"errCode":0,"data":{"total":0}}"#;
        let err = parse_page(body, 20).unwrap_err();
        assert_eq!(err.code, Code::SchemaChanged);
    }

    #[test]
    fn nested_list_under_data_wrapper() {
        let body = r#"{"data":{"resp":{"postList":[{"feedId":"f1","title":"t1","createTimeMs":1790000000000}]},"has_more":false}}"#;
        let page = parse_page(body, 20).unwrap();
        assert!(!page.has_more);
        assert_eq!(page.entries[0].id, "f1");
        assert!(page.entries[0].created_at.is_some());
    }

    #[test]
    fn garbage_body_is_schema_changed() {
        let err = parse_page("<html>not json</html>", 20).unwrap_err();
        assert_eq!(err.code, Code::SchemaChanged);
    }

    #[test]
    fn parses_collection_list() {
        // 形状对齐 2026-09-26 sniff：data.collectionList[] + collectionListCount
        let body = r#"{"errCode":0,"data":{"collectionList":[
            {"id":"event/abc","name":"机械系列","feedCount":54},
            {"id":"event/def","name":"儿童科普","feedCount":2}
        ],"collectionListCount":2,"baseResp":{"errcode":0}}}"#;
        let (cols, total) = parse_collections_body(body).unwrap();
        assert_eq!(total, 2);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].id, "event/abc");
        assert_eq!(cols[0].name, "机械系列");
        assert_eq!(cols[0].feed_count, 54);
    }

    #[test]
    fn parses_collection_feeds() {
        // 形状对齐 2026-09-26 collection 探针：objectInfo[] = {feed, sortId}，
        // feed 与 post_list item 同形（desc 嵌套），total=collectionFeedCount
        let body = r#"{"errCode":0,"data":{"objectInfo":[
            {"feed":{"objectId":"export/A","createTime":1790000000,"desc":{"shortTitle":"齿轮","description":"d1","media":[{"coverUrl":"https://cdn/a.jpg"}]},"readCount":9},"sortId":1},
            {"feed":{"objectId":"export/B","desc":{"description":"d2"}},"sortId":2}
        ],"collectionFeedCount":10,"collectionInfo":{"name":"机械系列"}}}"#;
        let (entries, total) = parse_collection_feed_body(body).unwrap();
        assert_eq!(total, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "export/A");
        assert_eq!(entries[0].title, "齿轮");
        assert_eq!(entries[0].read_count, Some(9));
        assert_eq!(entries[1].description.as_deref(), Some("d2"));
    }

    #[test]
    fn collection_feed_shape_drift_is_loud() {
        let body = r#"{"errCode":0,"data":{"feeds":[]}}"#;
        let err = parse_collection_feed_body(body).unwrap_err();
        assert_eq!(err.code, Code::SchemaChanged);
    }
}
