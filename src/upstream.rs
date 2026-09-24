//! 两步解析链（契约核对自上游 ltaoo/wx_channels_download@124f044）：
//!
//!  1. POST https://yuanbao.tencent.com/api/weixin/get_parse_result → data.playable_url
//!  2. POST https://channels.weixin.qq.com/finder-preview/api/feed/get_feed_info
//!     → data.feedInfo.* 视频地址选择
//!
//! Resolve 是唯一校验器：login 原样复用它。

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::apperr::{AppError, Code, Result, Stage};
use crate::auth::Credentials;
use crate::http::{read_bounded, CancelToken, RawRequest, RoundTrip};
use crate::media::{self, ResolvedVideo};
use crate::netpolicy;

pub const DEFAULT_YUANBAO_BASE: &str = "https://yuanbao.tencent.com";
pub const DEFAULT_FINDER_BASE: &str = "https://channels.weixin.qq.com";

const YUANBAO_PARSE_PATH: &str = "/api/weixin/get_parse_result";
const FINDER_FEED_PATH: &str = "/finder-preview/api/feed/get_feed_info";
const FINDER_PAGE_PATH: &str = "/finder-preview/pages/feed";

/// 固定的工具标识。用户自己采集的 User-Agent 导入后会覆盖它。
pub const DEFAULT_USER_AGENT: &str = "sph-local/1.0";

pub const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_API_BODY_BYTES: usize = 4 << 20; // 4 MiB
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// 执行解析链的客户端。Base URL 可被测试覆盖。
pub struct Client {
    pub http: Arc<dyn RoundTrip>,
    pub yuanbao_base: String,
    pub finder_base: String,
    /// 测试可替换的退避睡眠。
    pub sleep: Option<Box<dyn Fn(Duration) + Send + Sync>>,
    pub cancel: CancelToken,
}

impl Client {
    pub fn new(http: Arc<dyn RoundTrip>) -> Client {
        Client {
            http,
            yuanbao_base: DEFAULT_YUANBAO_BASE.into(),
            finder_base: DEFAULT_FINDER_BASE.into(),
            sleep: None,
            cancel: CancelToken::default(),
        }
    }

    fn sleep(&self, d: Duration) {
        if let Some(s) = &self.sleep {
            s(d);
        } else {
            std::thread::sleep(d);
        }
    }

    /// 对一条分享链接跑完整解析链。它是唯一校验器：login 原样复用。
    pub async fn resolve(&self, share_url: &str, creds: &Credentials) -> Result<ResolvedVideo> {
        let normalized = netpolicy::normalize_share_url(share_url)?;
        let playable = self.parse_share(&normalized, creds).await?;
        let (token, eid) = extract_preview_params(&playable)?;
        let feed = self.fetch_feed(&token, &eid, creds).await?;
        let mut video = select_media(&feed)?;
        video.share_url = normalized.clone();
        video.local_id = media::local_id(&normalized);
        Ok(video)
    }

    // ── 第 1 步：元宝解析 ─────────────────────────────────────────

    async fn parse_share(&self, normalized_url: &str, creds: &Credentials) -> Result<String> {
        let body = json!({
            "type": "video_channel_url",
            "url": normalized_url,
            "scene": 1,
        })
        .to_string()
        .into_bytes();
        let mut headers = vec![
            ("accept".into(), "application/json, text/plain, */*".into()),
            ("content-type".into(), "application/json".into()),
            ("origin".into(), self.yuanbao_base.clone()),
            ("referer".into(), format!("{}/", self.yuanbao_base)),
        ];
        // 导入的 UA 覆盖固定标识；其余额外请求头按白名单透传
        let mut ua = DEFAULT_USER_AGENT.to_string();
        for (k, v) in &creds.yuanbao_headers {
            if k == "user-agent" {
                ua = v.clone();
            } else {
                headers.push((k.clone(), v.clone()));
            }
        }
        headers.push(("user-agent".into(), ua));
        headers.push(("cookie".into(), creds.cookie.clone()));
        let req = RawRequest {
            method: "POST".into(),
            url: format!("{}{}", self.yuanbao_base, YUANBAO_PARSE_PATH),
            headers,
            body,
        };
        let mut resp = self
            .do_with_retry(Stage::ParseShare, "元宝解析", &req)
            .await?;
        map_api_status(resp.status, Stage::ParseShare, "元宝解析接口")?;
        let raw = read_bounded(&mut resp.body, MAX_API_BODY_BYTES, Stage::ParseShare).await?;
        let yr: Value = serde_json::from_slice(&raw).map_err(|_| {
            AppError::new(
                Code::SchemaChanged,
                Stage::ParseShare,
                "元宝响应不是有效 JSON",
            )
        })?;
        let code = yr.get("code").and_then(|c| c.as_i64()).ok_or_else(|| {
            AppError::new(
                Code::SchemaChanged,
                Stage::ParseShare,
                "元宝响应缺少数字 code 字段",
            )
        })?;
        if code != 0 {
            let msg = yr.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(AppError::fmt(
                Code::UpstreamError,
                Stage::ParseShare,
                format_args!("元宝业务错误 code {code}: {}", sanitize_message(msg)),
            ));
        }
        let playable = yr
            .get("data")
            .and_then(|d| d.get("playable_url"))
            .and_then(|p| p.as_str())
            .unwrap_or("");
        if playable.is_empty() {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::ParseShare,
                "元宝响应缺少 data.playable_url",
            ));
        }
        Ok(playable.to_string())
    }

    // ── 第 2 步：finder 预览 feed ────────────────────────────────

    async fn fetch_feed(
        &self,
        token: &str,
        eid: &str,
        creds: &Credentials,
    ) -> Result<FinderResponse> {
        let rid = build_rid();
        // query 键按字母序编码（Go url.Values.Encode 语义）
        let page_url = format!("{}{}", self.finder_base, FINDER_PAGE_PATH);
        let query = form_encode(&[("_pageUrl", page_url.as_str()), ("_rid", rid.as_str())]);
        let body = json!({
            "baseReq": { "generalToken": token },
            "exportId": eid,
        })
        .to_string()
        .into_bytes();
        // 只有非敏感的 User-Agent 可以从元宝会话复用；其余会话头与 Cookie 留在原地。
        let ua = creds
            .yuanbao_headers
            .get("user-agent")
            .filter(|v| !v.is_empty())
            .cloned()
            .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string());
        let referer = build_finder_referer(token, eid);
        let req = RawRequest {
            method: "POST".into(),
            url: format!("{}{}?{}", self.finder_base, FINDER_FEED_PATH, query),
            headers: vec![
                ("accept".into(), "application/json, text/plain, */*".into()),
                ("content-type".into(), "application/json".into()),
                ("origin".into(), self.finder_base.clone()),
                ("referer".into(), referer),
                ("user-agent".into(), ua),
            ],
            body,
        };
        let mut resp = self
            .do_with_retry(Stage::FetchFeed, "视频号预览", &req)
            .await?;
        map_api_status(resp.status, Stage::FetchFeed, "视频号预览接口")?;
        let raw = read_bounded(&mut resp.body, MAX_API_BODY_BYTES, Stage::FetchFeed).await?;
        parse_finder_response(&raw)
    }

    // ── 共享 HTTP 机制 ───────────────────────────────────────────

    /// "至多一次重试、共享截止时间"策略：仅瞬态网络失败与 502/503/504；
    /// 父级取消是终态。
    async fn do_with_retry(
        &self,
        stage: Stage,
        what: &str,
        req: &RawRequest,
    ) -> Result<crate::http::RawResponse> {
        let first = self.do_once(stage, what, req).await;
        if !should_retry(self.cancel.cancelled(), first.as_ref()) {
            return first;
        }
        self.sleep(RETRY_DELAY);
        if self.cancel.cancelled() {
            return first;
        }
        self.do_once(stage, what, req).await
    }

    async fn do_once(
        &self,
        stage: Stage,
        what: &str,
        req: &RawRequest,
    ) -> Result<crate::http::RawResponse> {
        if self.cancel.cancelled() {
            return Err(AppError::new(
                Code::Cancelled,
                stage,
                format!("{what}已取消"),
            ));
        }
        let resp = self.http.send(req.clone(), API_REQUEST_TIMEOUT).await?;
        if (300..400).contains(&resp.status) {
            return Err(AppError::fmt(
                Code::UpstreamError,
                stage,
                format_args!("{what}接口返回 HTTP {} 重定向，已拒绝跟随", resp.status),
            ));
        }
        Ok(resp)
    }
}

fn should_retry(
    cancelled: bool,
    result: std::result::Result<&crate::http::RawResponse, &AppError>,
) -> bool {
    if cancelled {
        return false;
    }
    match result {
        Ok(resp) => transient_http_status(resp.status),
        Err(e) => matches!(e.code, Code::Timeout | Code::NetworkError),
    }
}

fn transient_http_status(code: u16) -> bool {
    matches!(code, 502..=504)
}

/// API HTTP 状态归类：任何 2xx 都是成功（真实的 get_feed_info 曾被观察到返回
/// HTTP 201 且带合法 JSON 体，上游 worker.js 按 fetch 的 resp.ok 语义接受）。
/// 严格 200-only 保留给媒体响应。
fn map_api_status(status: u16, stage: Stage, what: &str) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        401 => Err(AppError::new(
            Code::InvalidCredentials,
            stage,
            "登录凭证不可用，请执行 sph login 重新登录。",
        )),
        403 => Err(AppError::new(
            Code::AccessDenied,
            stage,
            format!("{what}拒绝访问 (HTTP 403)"),
        )),
        429 => Err(AppError::new(
            Code::RateLimited,
            stage,
            format!("{what}限流 (HTTP 429)，请稍后重试"),
        )),
        _ => Err(AppError::fmt(
            Code::UpstreamError,
            stage,
            format_args!("{what}返回 HTTP {status}"),
        )),
    }
}

/// 从 playable_url 提取 generalToken 与 exportId：恰好一个 token、一个 eid，
/// 各经一次 query 解码后非空。
fn extract_preview_params(playable_url: &str) -> Result<(String, String)> {
    let u = netpolicy::validate_preview_url(playable_url)?;
    let counts = netpolicy::count_query_keys(u.query().unwrap_or(""));
    if counts.get("token").copied().unwrap_or(0) > 1 || counts.get("eid").copied().unwrap_or(0) > 1
    {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::ParseShare,
            "playable_url 的 token/eid 参数重复",
        ));
    }
    let q = u.query().unwrap_or("");
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(q.as_bytes())
        .into_owned()
        .collect();
    let token = pairs
        .iter()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let eid = pairs
        .iter()
        .find(|(k, _)| k == "eid")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if token.is_empty() || eid.is_empty() {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::ParseShare,
            "playable_url 缺少 token 或 eid",
        ));
    }
    Ok((token, eid))
}

fn build_rid() -> String {
    use rand::Rng;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut buf = [0u8; 4];
    rand::thread_rng().fill(&mut buf);
    format!("{ts:x}-{}", hex_encode(&buf))
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn build_finder_referer(token: &str, eid: &str) -> String {
    // Go url.Values.Encode：键按字母序
    let pairs = [
        ("appid", "0"),
        ("comment_scene", "39"),
        ("eid", eid),
        ("entry_card_type", "48"),
        ("entry_scene", "0"),
        ("token", token),
    ];
    format!(
        "https://channels.weixin.qq.com{}?{}",
        FINDER_PAGE_PATH,
        form_encode(&pairs)
    )
}

/// Go url.Values.Encode 语义：键排序、空格编码为 '+'、保留 [A-Za-z0-9-_.~] 以外的字符百分号编码。
/// （字母序由调用方保证。）
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<(&str, &str)> = pairs.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
        .iter()
        .map(|(k, v)| format!("{}={}", query_escape(k), query_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Go url.QueryEscape 语义：A-Za-z0-9-_.~ 之外全部百分号编码，空格为 '+'。
pub(crate) fn query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct FinderResponse {
    #[serde(rename = "errCode")]
    pub err_code: Option<i64>, // 必须为数字，缺失视为结构变化
    #[serde(rename = "errMsg")]
    pub err_msg: Option<Value>,
    pub data: Option<FinderResponseData>,
}

#[derive(Debug, Deserialize)]
pub struct FinderResponseData {
    #[serde(rename = "errMsg")]
    pub err_msg: Option<FinderErrMsg>,
    #[serde(rename = "feedInfo")]
    pub feed_info: Option<FeedInfo>,
    #[serde(rename = "authorInfo")]
    pub author_info: Option<AuthorInfo>,
}

#[derive(Debug, Deserialize)]
pub struct FinderErrMsg {
    #[serde(rename = "type")]
    pub msg_type: i64,
    pub title: Option<String>,
    pub content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeedInfo {
    #[serde(rename = "h264VideoInfo")]
    pub h264_video_info: Option<VideoInfo>,
    #[serde(rename = "h265VideoInfo")]
    pub h265_video_info: Option<VideoInfo>,
    #[serde(rename = "videoUrl")]
    pub video_url: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "picInfo")]
    pub pic_info: Option<Vec<PicInfo>>,
}

#[derive(Debug, Deserialize)]
pub struct VideoInfo {
    #[serde(rename = "videoUrl")]
    pub video_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PicInfo {
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorInfo {
    pub nickname: Option<String>,
}

fn parse_finder_response(raw: &[u8]) -> Result<FinderResponse> {
    let fr: FinderResponse = serde_json::from_slice(raw).map_err(|_| {
        AppError::new(
            Code::SchemaChanged,
            Stage::FetchFeed,
            "预览接口响应不是有效 JSON",
        )
    })?;
    let err_code = fr.err_code.ok_or_else(|| {
        AppError::new(
            Code::SchemaChanged,
            Stage::FetchFeed,
            "预览接口响应缺少数字 errCode",
        )
    })?;
    if err_code != 0 {
        // 未知的数字码保持数字形式以便诊断；绝不猜测成 "cookie 过期"。
        let msg = fr
            .err_msg
            .as_ref()
            .and_then(|m| m.as_str())
            .map(sanitize_message)
            .unwrap_or_default();
        return Err(AppError::fmt(
            Code::UpstreamError,
            Stage::FetchFeed,
            format_args!("预览接口业务错误 errCode {err_code}: {msg}"),
        ));
    }
    if let Some(em) = fr.data.as_ref().and_then(|d| d.err_msg.as_ref()) {
        let title = em
            .title
            .as_deref()
            .map(sanitize_message)
            .unwrap_or_default();
        let content = em
            .content
            .as_deref()
            .map(sanitize_message)
            .unwrap_or_default();
        let combined = format!("{title}: {content}")
            .trim_matches(':')
            .trim()
            .to_string();
        let msg = if combined.is_empty() {
            format!("视频不可用 (type {})", em.msg_type)
        } else {
            combined
        };
        return Err(AppError::new(
            Code::VideoUnavailable,
            Stage::FetchFeed,
            format!("视频不可用: {msg}"),
        ));
    }
    Ok(fr)
}

/// 按上游页面顺序挑选第一个非空地址：h264 → h265 → videoUrl。选中的 URL 原样使用。
fn select_media(fr: &FinderResponse) -> Result<ResolvedVideo> {
    let fi = fr
        .data
        .as_ref()
        .and_then(|d| d.feed_info.as_ref())
        .ok_or_else(|| {
            AppError::new(
                Code::NoMedia,
                Stage::SelectMedia,
                "响应中没有可用的视频信息",
            )
        })?;
    let pick = |url: Option<&String>, source: &str, codec: &str| -> Option<ResolvedVideo> {
        let url = url?.clone();
        if url.is_empty() {
            return None;
        }
        if netpolicy::validate_media_url(&url).is_err() {
            // 畸形地址是结构问题，不是回退触发条件
            return None;
        }
        Some(ResolvedVideo {
            media_url: url,
            media_source: source.into(),
            codec_hint: codec.into(),
            ..Default::default()
        })
    };
    if let Some(h264) = &fi.h264_video_info {
        if let Some(v) = pick(h264.video_url.as_ref(), "h264VideoInfo", "h264") {
            return Ok(with_meta(v, fi, fr));
        }
        if h264
            .video_url
            .as_deref()
            .map(|u| !u.is_empty())
            .unwrap_or(false)
        {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::SelectMedia,
                "h264 视频地址不合法",
            ));
        }
    }
    if let Some(h265) = &fi.h265_video_info {
        if let Some(v) = pick(h265.video_url.as_ref(), "h265VideoInfo", "h265") {
            return Ok(with_meta(v, fi, fr));
        }
        if h265
            .video_url
            .as_deref()
            .map(|u| !u.is_empty())
            .unwrap_or(false)
        {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::SelectMedia,
                "h265 视频地址不合法",
            ));
        }
    }
    if let Some(url) = &fi.video_url {
        if let Some(v) = pick(Some(url), "videoUrl", "") {
            return Ok(with_meta(v, fi, fr));
        }
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::SelectMedia,
            "视频地址不合法",
        ));
    }
    if fi.pic_info.as_ref().map(|p| !p.is_empty()).unwrap_or(false) {
        return Err(AppError::new(
            Code::UnsupportedMedia,
            Stage::SelectMedia,
            "该内容是图集，本版本不支持",
        ));
    }
    Err(AppError::new(
        Code::NoMedia,
        Stage::SelectMedia,
        "没有可用的视频地址",
    ))
}

fn with_meta(mut v: ResolvedVideo, fi: &FeedInfo, fr: &FinderResponse) -> ResolvedVideo {
    v.title = fi.description.clone().unwrap_or_default();
    if let Some(a) = fr.data.as_ref().and_then(|d| d.author_info.as_ref()) {
        v.author = a.nickname.clone().unwrap_or_default();
    }
    v
}

/// 让上游提供的消息可安全打印：剥掉控制字符与 HTML 标签，长度截断。
pub fn sanitize_message(msg: &str) -> String {
    let mut out = String::new();
    let mut inside_tag = false;
    for r in msg.chars() {
        match r {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if inside_tag => {}
            r if (r as u32) < 0x20 || r == '\u{7f}' => {}
            r => out.push(r),
        }
    }
    let out = out.trim().to_string();
    let count = out.chars().count();
    if count > 120 {
        out.chars().take(120).collect::<String>() + "…"
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Credentials, Source};
    use crate::http::{RawResponse, RoundTrip};
    use std::sync::Mutex;
    use std::time::Duration;

    const SENTINEL_COOKIE: &str = "DO_NOT_LEAK_COOKIE_123";
    const SENTINEL_TOKEN: &str = "DO_NOT_LEAK_TOKEN_456";
    const SENTINEL_MEDIA: &str = "DO_NOT_LEAK_MEDIA_QUERY_789";
    const TEST_SHARE: &str = "https://weixin.qq.com/sph/AogyNMyA7L";

    const PLAYABLE_URL: &str = "https://channels.weixin.qq.com/finder-preview/pages/feed?entry_card_type=48&comment_scene=39&appid=0&token=DO_NOT_LEAK_TOKEN_456&entry_scene=0&eid=EXPORTID1";
    const MEDIA_URL: &str = "https://cdn.example.com/video.mp4?X-snsvideoflag=1&encfilekey=abc&sig=DO_NOT_LEAK_MEDIA_QUERY_789";

    fn fake_creds() -> Credentials {
        Credentials {
            saved_at: time::OffsetDateTime::now_utc(),
            source: Source::BrowserLogin,
            verified_at: None,
            cookie: SENTINEL_COOKIE.into(),
            yuanbao_headers: [("user-agent".to_string(), "UA-TEST/1".to_string())]
                .into_iter()
                .collect(),
        }
    }

    #[derive(Clone)]
    struct Captured {
        url: String,
        #[allow(dead_code)]
        method: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    type Handler = Box<dyn Fn(&RawRequest) -> RawResponse + Send + Sync>;

    struct FakeTransport {
        state: Mutex<FakeState>,
    }

    struct FakeState {
        requests: Vec<Captured>,
        yuanbao: Option<Handler>,
        finder: Option<Handler>,
        fail: Option<String>, // 模拟瞬态网络错误
        fail_times: usize,
    }

    impl FakeTransport {
        fn new() -> FakeTransport {
            FakeTransport {
                state: Mutex::new(FakeState {
                    requests: vec![],
                    yuanbao: None,
                    finder: None,
                    fail: None,
                    fail_times: 0,
                }),
            }
        }

        fn yuanbao(self, f: impl Fn(&RawRequest) -> RawResponse + Send + Sync + 'static) -> Self {
            self.state.lock().unwrap().yuanbao = Some(Box::new(f));
            self
        }

        fn finder(self, f: impl Fn(&RawRequest) -> RawResponse + Send + Sync + 'static) -> Self {
            self.state.lock().unwrap().finder = Some(Box::new(f));
            self
        }

        fn fail_times(self, msg: &str, n: usize) -> Self {
            {
                let mut s = self.state.lock().unwrap();
                s.fail = Some(msg.into());
                s.fail_times = n;
            }
            self
        }

        fn seen(&self) -> Vec<Captured> {
            self.state.lock().unwrap().requests.clone()
        }
    }

    fn json_resp(status: u16, body: String) -> RawResponse {
        RawResponse {
            status,
            content_length: Some(body.len() as i64),
            body: Box::pin(std::io::Cursor::new(body.into_bytes())),
        }
    }

    #[async_trait::async_trait]
    impl RoundTrip for FakeTransport {
        async fn send(&self, req: RawRequest, _timeout: Duration) -> Result<RawResponse> {
            let mut s = self.state.lock().unwrap();
            if s.fail_times > 0 {
                s.fail_times -= 1;
                let msg = s.fail.clone().unwrap_or_default();
                drop(s);
                return Err(AppError::retryable(
                    Code::NetworkError,
                    Stage::Arguments,
                    msg,
                ));
            }
            s.requests.push(Captured {
                url: req.url.clone(),
                method: req.method.clone(),
                headers: req.headers.clone(),
                body: String::from_utf8_lossy(&req.body).into_owned(),
            });
            let is_yuanbao = req.url.contains("yuanbao");
            let is_finder = req.url.contains("channels.weixin.qq.com");
            let resp = if is_yuanbao {
                s.yuanbao.as_ref().map(|f| f(&req))
            } else if is_finder {
                s.finder.as_ref().map(|f| f(&req))
            } else {
                None
            };
            match resp {
                Some(r) => Ok(r),
                None => Ok(json_resp(404, r#"{"error":"no route"}"#.into())),
            }
        }
    }

    fn playable_resp(url: &str) -> RawResponse {
        json_resp(
            200,
            format!(r#"{{"code":0,"msg":"","data":{{"playable_url":"{url}"}}}}"#),
        )
    }

    fn feed_resp(video_url: &str) -> RawResponse {
        json_resp(
            200,
            format!(
                r#"{{"errCode":0,"errMsg":"","data":{{"feedInfo":{{"h264VideoInfo":{{"videoUrl":"{video_url}"}},"description":"标题","picInfo":[]}},"authorInfo":{{"nickname":"作者"}}}}}}"#
            ),
        )
    }

    fn test_client(f: Arc<FakeTransport>) -> Client {
        let mut c = Client::new(f);
        c.sleep = Some(Box::new(|_| {}));
        c
    }

    #[test]
    fn rid_format() {
        let rid = build_rid();
        let (ts, rand) = rid.split_once('-').expect("rid must contain -");
        assert!(!ts.is_empty());
        assert!(
            i64::from_str_radix(ts, 16).is_ok(),
            "ts part must be hex unix seconds"
        );
        assert_eq!(rand.len(), 8, "random part must be 8 hex chars");
    }

    #[tokio::test]
    async fn happy_path_and_credential_isolation() {
        let f = Arc::new(
            FakeTransport::new()
                .yuanbao(move |_| playable_resp(PLAYABLE_URL))
                .finder(move |_| feed_resp(MEDIA_URL)),
        );
        let client = test_client(f.clone());
        let video = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
        assert_eq!(video.media_url, MEDIA_URL, "媒体 URL 必须原样保留");
        assert_eq!(video.media_source, "h264VideoInfo");
        assert_eq!(video.codec_hint, "h264");
        assert_eq!(video.title, "标题");
        assert_eq!(video.author, "作者");
        assert_eq!(video.local_id.len(), 12);

        let seen = f.seen();
        assert_eq!(seen.len(), 2);
        let yb = &seen[0];
        assert_eq!(yb.header("cookie"), Some(SENTINEL_COOKIE));
        assert_eq!(
            yb.header("user-agent"),
            Some("UA-TEST/1"),
            "导入的 UA 应覆盖"
        );
        assert_eq!(yb.header("origin"), Some("https://yuanbao.tencent.com"));
        assert_eq!(yb.header("referer"), Some("https://yuanbao.tencent.com/"));
        assert!(yb.url.contains("/api/weixin/get_parse_result"));
        let yb_body: Value = serde_json::from_str(&yb.body).unwrap();
        assert_eq!(yb_body["type"], "video_channel_url");
        assert_eq!(yb_body["scene"], 1);

        let fd = &seen[1];
        assert_eq!(fd.header("cookie"), None, "finder 请求不得携带 cookie");
        assert_eq!(
            fd.header("user-agent"),
            Some("UA-TEST/1"),
            "finder 只复用 UA"
        );
        assert_eq!(fd.header("t-userid"), None, "finder 不得携带元宝会话头");
        assert!(fd.url.contains("/finder-preview/api/feed/get_feed_info"));
        assert!(fd.url.contains("_rid="));
        assert!(fd.url.contains("_pageUrl="));
        let fd_body: Value = serde_json::from_str(&fd.body).unwrap();
        assert_eq!(fd_body["baseReq"]["generalToken"], SENTINEL_TOKEN);
        assert_eq!(fd_body["exportId"], "EXPORTID1");
        let referer = fd.header("referer").unwrap();
        assert!(referer.contains(&format!("token={SENTINEL_TOKEN}")));
        assert!(referer.contains("eid=EXPORTID1"));
        assert!(referer.starts_with("https://channels.weixin.qq.com/finder-preview/pages/feed?"));
    }

    #[tokio::test]
    async fn accepts_2xx_non_200() {
        let f = FakeTransport::new()
            .yuanbao(move |_| {
                json_resp(
                    201,
                    format!(r#"{{"code":0,"data":{{"playable_url":"{PLAYABLE_URL}"}}}}"#),
                )
            })
            .finder(move |_| {
                json_resp(
                    201,
                    r#"{"errCode":0,"data":{"feedInfo":{"videoUrl":"https://cdn.example.com/v.mp4?a=1"}}}"#.into(),
                )
            });
        let media = "https://cdn.example.com/v.mp4?a=1";
        let client = test_client(std::sync::Arc::new(f));
        let video = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
        assert_eq!(video.media_url, media);
    }

    #[tokio::test]
    async fn error_matrix() {
        struct Case {
            name: &'static str,
            yuanbao: Option<fn(&RawRequest) -> RawResponse>,
            finder: Option<fn(&RawRequest) -> RawResponse>,
            want_code: Code,
            want_stage: Stage,
        }
        let cases: Vec<Case> = vec![
            Case {
                name: "yuanbao 401",
                yuanbao: Some(|_| json_resp(401, "{}".into())),
                finder: None,
                want_code: Code::InvalidCredentials,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "yuanbao 403",
                yuanbao: Some(|_| json_resp(403, "{}".into())),
                finder: None,
                want_code: Code::AccessDenied,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "yuanbao 429",
                yuanbao: Some(|_| json_resp(429, "{}".into())),
                finder: None,
                want_code: Code::RateLimited,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "yuanbao 302",
                yuanbao: Some(|_| json_resp(302, "{}".into())),
                finder: None,
                want_code: Code::UpstreamError,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "business code nonzero",
                yuanbao: Some(|_| json_resp(200, r#"{"code":1001,"msg":"need login"}"#.into())),
                finder: None,
                want_code: Code::UpstreamError,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "code missing",
                yuanbao: Some(|_| json_resp(200, r#"{"data":{"playable_url":"x"}}"#.into())),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "code wrong type",
                yuanbao: Some(|_| json_resp(200, r#"{"code":"0","data":{}}"#.into())),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "playable_url missing",
                yuanbao: Some(|_| json_resp(200, r#"{"code":0,"data":{}}"#.into())),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "playable_url wrong host",
                yuanbao: Some(|_| {
                    playable_resp("https://evil.test/finder-preview/pages/feed?token=a&eid=b")
                }),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "playable_url wrong path",
                yuanbao: Some(|_| {
                    playable_resp("https://channels.weixin.qq.com/other/path?token=a&eid=b")
                }),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "token missing",
                yuanbao: Some(|_| {
                    playable_resp("https://channels.weixin.qq.com/finder-preview/pages/feed?eid=b")
                }),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "eid missing",
                yuanbao: Some(|_| {
                    playable_resp(
                        "https://channels.weixin.qq.com/finder-preview/pages/feed?token=a",
                    )
                }),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "duplicate token",
                yuanbao: Some(|_| {
                    playable_resp("https://channels.weixin.qq.com/finder-preview/pages/feed?token=a&token=b&eid=c")
                }),
                finder: None,
                want_code: Code::SchemaChanged,
                want_stage: Stage::ParseShare,
            },
            Case {
                name: "finder errCode nonzero",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| {
                    json_resp(200, r#"{"errCode":-200,"errMsg":"<b>gone</b>"}"#.into())
                }),
                want_code: Code::UpstreamError,
                want_stage: Stage::FetchFeed,
            },
            Case {
                name: "finder errCode missing",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| json_resp(200, r#"{"data":{}}"#.into())),
                want_code: Code::SchemaChanged,
                want_stage: Stage::FetchFeed,
            },
            Case {
                name: "finder errMsg unavailable",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| {
                    json_resp(
                        200,
                        r#"{"errCode":0,"data":{"errMsg":{"type":1,"title":"视频","content":"已删除"}}}"#.into(),
                    )
                }),
                want_code: Code::VideoUnavailable,
                want_stage: Stage::FetchFeed,
            },
            Case {
                name: "finder not JSON",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| json_resp(200, "<html>oops</html>".into())),
                want_code: Code::SchemaChanged,
                want_stage: Stage::FetchFeed,
            },
            Case {
                name: "finder oversize",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| {
                    json_resp(
                        200,
                        format!(
                            r#"{{"errCode":0,"pad":"{}"}}"#,
                            "x".repeat(MAX_API_BODY_BYTES + 1)
                        ),
                    )
                }),
                want_code: Code::UpstreamError,
                want_stage: Stage::FetchFeed,
            },
            Case {
                name: "no feedInfo",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| json_resp(200, r#"{"errCode":0,"data":{}}"#.into())),
                want_code: Code::NoMedia,
                want_stage: Stage::SelectMedia,
            },
            Case {
                name: "pic album",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| {
                    json_resp(
                        200,
                        r#"{"errCode":0,"data":{"feedInfo":{"picInfo":[{"url":"https://x/1.jpg"}]}}}"#.into(),
                    )
                }),
                want_code: Code::UnsupportedMedia,
                want_stage: Stage::SelectMedia,
            },
            Case {
                name: "no media at all",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| {
                    json_resp(
                        200,
                        r#"{"errCode":0,"data":{"feedInfo":{"picInfo":[]}}}"#.into(),
                    )
                }),
                want_code: Code::NoMedia,
                want_stage: Stage::SelectMedia,
            },
            Case {
                name: "malformed media url",
                yuanbao: Some(|_| playable_resp(PLAYABLE_URL)),
                finder: Some(|_| {
                    json_resp(
                        200,
                        r#"{"errCode":0,"data":{"feedInfo":{"videoUrl":"http://not-https/x.mp4"}}}"#.into(),
                    )
                }),
                want_code: Code::SchemaChanged,
                want_stage: Stage::SelectMedia,
            },
        ];
        let secrets = [
            SENTINEL_COOKIE,
            SENTINEL_TOKEN,
            SENTINEL_MEDIA,
            PLAYABLE_URL,
            MEDIA_URL,
        ];
        for case in cases {
            let mut f = FakeTransport::new();
            if let Some(yb) = case.yuanbao {
                f = f.yuanbao(yb);
            }
            if let Some(fd) = case.finder {
                f = f.finder(fd);
            }
            let client = test_client(std::sync::Arc::new(f));
            let err = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap_err();
            assert_eq!(err.code, case.want_code, "{}: code", case.name);
            assert_eq!(err.stage, case.want_stage, "{}: stage", case.name);
            for s in secrets {
                assert!(
                    !err.message.contains(s),
                    "{}: 错误消息泄漏了秘密",
                    case.name
                );
                assert!(
                    !format!("{err}").contains(s),
                    "{}: Display 泄漏了秘密",
                    case.name
                );
            }
        }
    }

    #[tokio::test]
    async fn media_selection_order() {
        struct Case {
            name: &'static str,
            feed: String,
            want_source: &'static str,
            want_codec: &'static str,
        }
        let urls = [
            "https://a.example/v.mp4?x=1",
            "https://b.example/v.mp4?x=2",
            "https://c.example/v.mp4?x=3",
        ];
        let cases = vec![
            Case {
                name: "h264 first",
                feed: format!(
                    r#"{{"errCode":0,"data":{{"feedInfo":{{"h264VideoInfo":{{"videoUrl":"{}"}},"h265VideoInfo":{{"videoUrl":"{}"}},"videoUrl":"{}"}}}}}}"#,
                    urls[0], urls[1], urls[2]
                ),
                want_source: "h264VideoInfo",
                want_codec: "h264",
            },
            Case {
                name: "h265 fallback",
                feed: format!(
                    r#"{{"errCode":0,"data":{{"feedInfo":{{"h265VideoInfo":{{"videoUrl":"{}"}},"videoUrl":"{}"}}}}}}"#,
                    urls[1], urls[2]
                ),
                want_source: "h265VideoInfo",
                want_codec: "h265",
            },
            Case {
                name: "videoUrl fallback",
                feed: format!(
                    r#"{{"errCode":0,"data":{{"feedInfo":{{"videoUrl":"{}"}}}}}}"#,
                    urls[2]
                ),
                want_source: "videoUrl",
                want_codec: "",
            },
        ];
        for case in cases {
            let feed = case.feed.clone();
            let f = std::sync::Arc::new(
                FakeTransport::new()
                    .yuanbao(move |_| playable_resp(PLAYABLE_URL))
                    .finder(move |_| json_resp(200, feed.clone())),
            );
            let client = test_client(f);
            let video = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
            assert_eq!(video.media_source, case.want_source, "{}", case.name);
            assert_eq!(video.codec_hint, case.want_codec, "{}", case.name);
        }
        // 可选元数据缺失不致命
        let f = std::sync::Arc::new(
            FakeTransport::new()
                .yuanbao(move |_| playable_resp(PLAYABLE_URL))
                .finder(move |_| {
                    json_resp(
                        200,
                        r#"{"errCode":0,"data":{"feedInfo":{"videoUrl":"https://x.example/v.mp4"}}}"#.into(),
                    )
                }),
        );
        let client = test_client(f);
        let video = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
        assert_eq!(video.title, "");
        assert_eq!(video.author, "");
    }

    #[tokio::test]
    async fn media_url_fidelity() {
        let raw = "https://cdn.example.com/v.mp4?X-snsvideoflag=1&encfilekey=a%2Fb&sig=x%26y&sig=z&q=1&q=2";
        let f = std::sync::Arc::new(
            FakeTransport::new()
                .yuanbao(move |_| playable_resp(PLAYABLE_URL))
                .finder(move |_| feed_resp(raw)),
        );
        let client = test_client(f);
        let video = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
        assert_eq!(video.media_url, raw, "URL 不得被改写");
    }

    #[tokio::test]
    async fn retry_policy() {
        // 503 后成功 → 恰好重试一次
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = calls.clone();
        let sleeps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sleeps2 = sleeps.clone();
        let f = std::sync::Arc::new(
            FakeTransport::new()
                .yuanbao(move |_| {
                    let n = calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n == 0 {
                        json_resp(503, "down".into())
                    } else {
                        playable_resp(PLAYABLE_URL)
                    }
                })
                .finder(move |_| feed_resp(MEDIA_URL)),
        );
        let mut client = test_client(f);
        client.sleep = Some(Box::new(move |_| {
            sleeps2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            sleeps.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "503 后应退避一次"
        );

        // 403 不重试
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = calls.clone();
        let f = std::sync::Arc::new(FakeTransport::new().yuanbao(move |_| {
            calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            json_resp(403, "{}".into())
        }));
        let client = test_client(f);
        let err = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap_err();
        assert_eq!(err.code, Code::AccessDenied);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 瞬态网络错误后成功 → 重试一次
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = calls.clone();
        let f = FakeTransport::new()
            .yuanbao(move |_| {
                calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                playable_resp(PLAYABLE_URL)
            })
            .finder(move |_| feed_resp(MEDIA_URL));
        let f = f.fail_times("connection reset by peer", 1);
        let f = std::sync::Arc::new(f);
        let client = test_client(f);
        client.resolve(TEST_SHARE, &fake_creds()).await.unwrap();
    }

    #[tokio::test]
    async fn persistent_network_error_gives_up_cleanly() {
        let f =
            std::sync::Arc::new(FakeTransport::new().fail_times("dial tcp: connection refused", 2));
        let client = test_client(f);
        let err = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap_err();
        assert_eq!(err.code, Code::NetworkError);
        assert!(!err.message.contains("http"));
        assert!(!err.message.contains("yuanbao"));
    }

    #[tokio::test]
    async fn cancel_not_retried() {
        let f = std::sync::Arc::new(FakeTransport::new().fail_times("cancelled", 5));
        let client = test_client(f);
        client.cancel.cancel();
        let err = client.resolve(TEST_SHARE, &fake_creds()).await.unwrap_err();
        assert_eq!(err.code, Code::Cancelled);
    }

    #[tokio::test]
    async fn share_url_validated_before_any_request() {
        let f = std::sync::Arc::new(FakeTransport::new());
        let client = test_client(f.clone());
        let err = client
            .resolve("https://evil.test/x", &fake_creds())
            .await
            .unwrap_err();
        assert_eq!(err.code, Code::InvalidArgument);
        assert_eq!(f.seen().len(), 0);
    }

    #[test]
    fn sanitize_message_strips_tags_and_caps() {
        let got = sanitize_message("<b>登录已过期\u{1b}[31m红字\0</b>\n");
        assert!(!got.contains(['<', '>', '\u{1b}', '\0', '\n']));
        let long = sanitize_message(&"字".repeat(500));
        assert!(long.chars().count() <= 121);
    }

    // 辅助：给 Captured 加 header 查询
    impl Captured {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }
}
