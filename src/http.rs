//! 可注入的 HTTP 传输层：生产实现包 reqwest（忽略代理、拒绝非公网地址、
//! API 客户端不重定向、媒体客户端受控重定向），测试注入脚本化假实现。
//!
//! 请求/响应刻意使用自有类型，不泄漏 reqwest 类型，让测试离线可跑。

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::TryStreamExt;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::netpolicy;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const TLS_HANDSHAKE: Duration = Duration::from_secs(10);
pub const RESPONSE_HEADER_WAIT: Duration = Duration::from_secs(30);
pub const MAX_MEDIA_REDIRECTS: usize = 5;

/// 跨命令共享的取消令牌（Ctrl+C 置位）。
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 一个 HTTP 请求（自有类型，便于测试断言）。
#[derive(Debug, Clone)]
pub struct RawRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>, // 小写键
    pub body: Vec<u8>,
}

impl RawRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// 响应头 + 流式体。
pub struct RawResponse {
    pub status: u16,
    pub content_length: Option<i64>,
    pub body: Pin<Box<dyn tokio::io::AsyncRead + Send>>,
}

/// 传输层抽象：测试用脚本化假实现，生产用 reqwest。
#[async_trait]
pub trait RoundTrip: Send + Sync {
    async fn send(&self, req: RawRequest, per_attempt_timeout: Duration) -> Result<RawResponse>;
}

fn header_get<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

// ── 生产实现：reqwest ─────────────────────────────────────────────
/// 解析主机名并要求每个解析地址都是公网单播地址。
#[derive(Clone)]
struct PublicDnsResolver;

impl reqwest::dns::Resolve for PublicDnsResolver {
    fn resolve(
        &self,
        name: reqwest::dns::Name,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = std::result::Result<
                        Box<dyn Iterator<Item = SocketAddr> + Send>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send,
        >,
    > {
        Box::pin(resolve_public(name))
    }
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

async fn resolve_public(
    name: reqwest::dns::Name,
) -> std::result::Result<Box<dyn Iterator<Item = SocketAddr> + Send>, BoxErr> {
    let host = name.as_str().to_string();
    let mut addrs: Vec<SocketAddr> = Vec::new();
    let resolved = tokio::net::lookup_host(format!("{host}:0")).await?;
    for a in resolved {
        let ip = a.ip();
        if !is_public_ip(ip) {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "blocked-address",
            )));
        }
        addrs.push(a);
    }
    if addrs.is_empty() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "host resolved to no addresses",
        )));
    }
    Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
}

/// 公网单播地址判定：回环、私网、链路本地、组播、广播、"本网络"与未指定地址全部拒绝。
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_multicast()
                || v4.is_private()
                || v4.is_link_local()
                || o == [255, 255, 255, 255]
                || o[0] == 0)
        }
        IpAddr::V6(v6) => {
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                // IPv4 映射地址按 v4 规则判断
                || v6.to_ipv4_mapped().map(|v4| !is_public_ip(IpAddr::V4(v4))).unwrap_or(false)
                || (v6.segments()[0] & 0xffc0) == 0xfe80) // 链路本地
        }
    }
}

fn base_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy() // 忽略所有代理环境变量
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(RESPONSE_HEADER_WAIT)
        .dns_resolver(Arc::new(PublicDnsResolver))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(30))
}

/// API 端点（元宝/预览）专用客户端：永不跟随重定向。
pub fn new_api_client() -> reqwest::Client {
    base_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("api client build")
}

/// 媒体客户端：最多跟随 5 跳重定向，每一跳都用 validate_media_url 复核，
/// 会话头因此不可能泄漏到非预期目的地。无 Cookie jar。
pub fn new_media_client() -> reqwest::Client {
    base_builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_MEDIA_REDIRECTS {
                return attempt.error("媒体重定向超过 5 跳");
            }
            if netpolicy::validate_media_url(attempt.url().as_str()).is_err() {
                return attempt.error("媒体重定向目标被拒绝");
            }
            attempt.follow()
        }))
        .build()
        .expect("media client build")
}

/// Arc<dyn RoundTrip> 委托实现，让 Options/Client 可以持有类型擦除的传输层。
#[async_trait]
impl RoundTrip for Arc<dyn RoundTrip> {
    async fn send(&self, req: RawRequest, per_attempt_timeout: Duration) -> Result<RawResponse> {
        (**self).send(req, per_attempt_timeout).await
    }
}

/// reqwest 传输层实现。
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn api() -> ReqwestTransport {
        ReqwestTransport {
            client: new_api_client(),
        }
    }
    pub fn media() -> ReqwestTransport {
        ReqwestTransport {
            client: new_media_client(),
        }
    }
}

#[async_trait]
impl RoundTrip for ReqwestTransport {
    async fn send(&self, req: RawRequest, per_attempt_timeout: Duration) -> Result<RawResponse> {
        if per_attempt_timeout.is_zero() {
            return Err(AppError::new(Code::Timeout, Stage::Arguments, "请求超时"));
        }
        let stage = classify_stage(&req);
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| AppError::new(Code::InternalError, Stage::Arguments, "构造请求失败"))?;
        let mut rb = self.client.request(method, &req.url).body(req.body.clone());
        for (k, v) in &req.headers {
            rb = rb.header(k, v);
        }
        let fut = rb.send();
        let resp = tokio::time::timeout(per_attempt_timeout, fut)
            .await
            .map_err(|_| AppError::retryable(Code::Timeout, stage, "请求超时"))?
            .map_err(|e| map_reqwest_error(e, stage))?;
        let status = resp.status().as_u16();
        let content_length = resp.content_length().map(|v| v as i64);
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        let reader = tokio_util::io::StreamReader::new(stream);
        Ok(RawResponse {
            status,
            content_length,
            body: Box::pin(reader),
        })
    }
}

fn classify_stage(req: &RawRequest) -> Stage {
    if req.url.contains("get_parse_result") {
        Stage::ParseShare
    } else if req.url.contains("get_feed_info") {
        Stage::FetchFeed
    } else {
        Stage::Download
    }
}

/// reqwest 错误归类：不内嵌原始 URL 或错误文本。
pub fn map_reqwest_error(e: reqwest::Error, stage: Stage) -> AppError {
    use std::error::Error as _;
    if e.is_timeout() {
        return AppError::retryable(Code::Timeout, stage, "请求超时");
    }
    let inner = e
        .source()
        .map(|s| s.to_string())
        .unwrap_or_else(|| e.to_string());
    if inner.contains("blocked-address") {
        return AppError::new(
            Code::NetworkError,
            stage,
            "目标地址被本工具网络策略拒绝（不允许回环/私网地址）",
        );
    }
    if e.is_connect() {
        if inner.contains("dns error") || inner.contains("failed to lookup address") {
            return AppError::new(Code::NetworkError, stage, "域名解析失败");
        }
        return AppError::new(Code::NetworkError, stage, "网络连接失败");
    }
    if e.is_request() {
        // URL 解析失败等
        return AppError::new(Code::NetworkError, stage, "网络请求失败");
    }
    AppError::new(Code::NetworkError, stage, "网络请求失败")
}

/// 供错误信息使用的安全 URL 形式：剥掉 query 与 userinfo。
pub fn sanitize_url_for_error(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.as_str().to_string()
        }
        Err(_) => "<无法解析的 URL>".into(),
    }
}

/// 便捷头构建。
pub fn headers_of(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// 从响应读体的便捷封装：读满上限字节。
pub async fn read_bounded(
    body: &mut (impl tokio::io::AsyncRead + Unpin),
    limit: usize,
    stage: Stage,
) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let n = body
        .take((limit + 1) as u64)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| AppError::new(Code::NetworkError, stage, format!("读取响应失败: {e}")))?;
    let _ = n;
    if buf.len() > limit {
        return Err(AppError::fmt(
            Code::UpstreamError,
            stage,
            format_args!("响应超过 {} 字节上限", limit),
        ));
    }
    Ok(buf)
}

/// 统计后端响应里的头（测试用）。
pub fn debug_headers(req: &RawRequest) -> HashMap<String, String> {
    req.headers.iter().cloned().collect()
}

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    header_get(headers, name)
}
