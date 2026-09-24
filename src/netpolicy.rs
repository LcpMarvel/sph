//! 网络目标规则：严格的分享链接校验、媒体 URL 校验、拒绝非公网地址的解析器与重定向策略。

use std::collections::HashMap;
use std::net::IpAddr;

use crate::apperr::{AppError, Code, Result, Stage};

/// 分享链接的最大长度。
pub const MAX_SHARE_URL_BYTES: usize = 8192;

const SHARE_HOST: &str = "weixin.qq.com";
const SHARE_PATH_PREFIX: &str = "/sph/";

/// 校验视频号分享链接并返回归一化形式：丢弃 fragment，query 原样保留。
/// 归一化形式是 local_id 哈希的输入。
pub fn normalize_share_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "未提供分享链接",
        ));
    }
    if trimmed.len() > MAX_SHARE_URL_BYTES {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("链接超过 {} 字节上限", MAX_SHARE_URL_BYTES),
        ));
    }
    for r in trimmed.chars() {
        if r == '\n' || r == '\r' || r == '\t' || r.is_control() {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "链接包含换行或控制字符",
            ));
        }
    }
    let mut u = url::Url::parse(trimmed)
        .map_err(|_| AppError::new(Code::InvalidArgument, Stage::Arguments, "链接格式不合法"))?;
    if u.scheme() != "https" {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "链接必须是 HTTPS 协议",
        ));
    }
    if u.host_str() != Some(SHARE_HOST) {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("链接主机必须是 {}", SHARE_HOST),
        ));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "链接不允许携带 userinfo",
        ));
    }
    if u.port().is_some() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "链接不允许自定义端口",
        ));
    }
    // 在序列化形式上取原始（仍带百分号编码）路径：Go 校验的是 EscapedPath，
    // 编码后的字符（如 %20）必须被拒绝而不是被解码后放过。
    let serialized = u.as_str();
    let after_authority = match serialized.find("://") {
        Some(i) => {
            let rest = &serialized[i + 3..];
            match rest.find('/') {
                Some(j) => &rest[j..],
                None => "",
            }
        }
        None => {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "链接格式不合法",
            ))
        }
    };
    let path_raw = after_authority.split(['?', '#']).next().unwrap_or("");
    let Some(code) = path_raw.strip_prefix(SHARE_PATH_PREFIX) else {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "链接路径必须是 /sph/<短码>",
        ));
    };
    if code.is_empty() || code.contains('/') {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "链接路径必须是 /sph/<短码>",
        ));
    }
    for r in code.chars() {
        if !is_share_code_rune(r) {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "短码包含非法字符",
            ));
        }
    }
    u.set_fragment(None);
    Ok(u.as_str().to_string())
}

fn is_share_code_rune(r: char) -> bool {
    r.is_ascii_lowercase() || r.is_ascii_uppercase() || r.is_ascii_digit() || r == '_' || r == '-'
}

/// 按契约校验上游 API 返回的媒体 URL：HTTPS、无 userinfo、无自定义端口、
/// 不是 IP 字面量。原始字符串必须原样用于请求；本校验不改写它。
pub fn validate_media_url(raw: &str) -> Result<()> {
    let u = url::Url::parse(raw)
        .map_err(|_| AppError::new(Code::SchemaChanged, Stage::SelectMedia, "媒体地址不合法"))?;
    if u.scheme() != "https" {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::SelectMedia,
            "媒体地址必须是 HTTPS",
        ));
    }
    match u.host_str() {
        Some(h) if !h.is_empty() => {}
        _ => {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::SelectMedia,
                "媒体地址缺少主机名",
            ))
        }
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::SelectMedia,
            "媒体地址不允许携带 userinfo",
        ));
    }
    if u.port().is_some() {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::SelectMedia,
            "媒体地址不允许自定义端口",
        ));
    }
    if let Ok(ip) = raw_host_ip(&u) {
        let _ = ip;
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::SelectMedia,
            "媒体地址不允许是 IP 字面量",
        ));
    }
    Ok(())
}

fn raw_host_ip(u: &url::Url) -> std::result::Result<IpAddr, ()> {
    // host_str 对 IPv6 字面量会带方括号，剥掉再解析
    u.host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .map_err(|_| ())
}

/// 校验 playable_url：必须是 channels.weixin.qq.com 的 finder 预览页地址。
pub fn validate_preview_url(raw: &str) -> Result<url::Url> {
    let u = url::Url::parse(raw).map_err(|_| {
        AppError::new(
            Code::SchemaChanged,
            Stage::ParseShare,
            "playable_url 不是预期的视频号预览地址",
        )
    })?;
    if u.scheme() != "https" || u.host_str() != Some("channels.weixin.qq.com") {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::ParseShare,
            "playable_url 不是预期的视频号预览地址",
        ));
    }
    if !u.username().is_empty() || u.password().is_some() || u.port().is_some() {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::ParseShare,
            "playable_url 不允许携带 userinfo 或自定义端口",
        ));
    }
    if u.path() != "/finder-preview/pages/feed" {
        return Err(AppError::new(
            Code::SchemaChanged,
            Stage::ParseShare,
            "playable_url 路径结构已变化",
        ));
    }
    Ok(u)
}

/// 统计原始 query 串中每个键出现的次数。
pub fn count_query_keys(raw_query: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let key = pair.split('=').next().unwrap_or(pair);
        let decoded = form_urldecode(key);
        *counts.entry(decoded).or_insert(0) += 1;
    }
    counts
}

/// Go url.QueryUnescape 语义：'+' 解码为空格。
pub fn form_urldecode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts() {
        let cases: Vec<(&str, &str)> = vec![
            (
                "https://weixin.qq.com/sph/AogyNMyA7L",
                "https://weixin.qq.com/sph/AogyNMyA7L",
            ),
            (
                "  https://weixin.qq.com/sph/abc-123_X  ",
                "https://weixin.qq.com/sph/abc-123_X",
            ),
            (
                "https://weixin.qq.com/sph/code?ch=a&x=1",
                "https://weixin.qq.com/sph/code?ch=a&x=1",
            ),
            (
                "https://weixin.qq.com/sph/code#share",
                "https://weixin.qq.com/sph/code",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_share_url(input).unwrap(), want, "input {input:?}");
        }
    }

    #[test]
    fn normalize_rejects() {
        let long = "a".repeat(8192);
        let cases: Vec<String> = vec![
            "".into(),
            "   ".into(),
            "http://weixin.qq.com/sph/a".into(),
            "https://weixin.qq.com.evil.test/sph/a".into(),
            "https://evil.test/sph/a".into(),
            "https://user:pass@weixin.qq.com/sph/a".into(),
            "https://weixin.qq.com@evil.test/sph/a".into(),
            "https://weixin.qq.com:8443/sph/a".into(),
            "https://weixin.qq.com/sph/".into(),
            "https://weixin.qq.com/sph/a/b".into(),
            "https://weixin.qq.com/other/a".into(),
            "https://weixin.qq.com/sph/打".into(),
            "https://weixin.qq.com/sph/a%20b".into(),
            "https://weixin.qq.com/sph/a\nb".into(),
            "https://weixin.qq.com/sph/a b".into(),
            "javascript:alert(1)".into(),
            "file:///etc/passwd".into(),
            "https://weixin.qq.com/sph/a https://weixin.qq.com/sph/b".into(),
            format!("https://weixin.qq.com/sph/{long}"),
            "https://weixin.qq.com/sph/a\0b".into(),
        ];
        for input in cases {
            let err = normalize_share_url(&input).expect_err(&format!("must reject {input:?}"));
            assert_eq!(err.code, Code::InvalidArgument, "input {input:?}");
        }
    }

    #[test]
    fn validate_media_url_matrix() {
        for u in [
            "https://example.com/v.mp4?X-snsvideoflag=1&encfilekey=abc",
            "https://cdn.example.com/a/b/c.mp4",
        ] {
            assert!(validate_media_url(u).is_ok(), "{u}");
        }
        for u in [
            "http://example.com/v.mp4",
            "https://1.2.3.4/v.mp4",
            "https://user@example.com/v.mp4",
            "https://example.com:8443/v.mp4",
            "/relative",
            "",
            "https://[::1]/v.mp4",
        ] {
            assert!(validate_media_url(u).is_err(), "{u}");
        }
    }

    #[test]
    fn count_query_keys_works() {
        let c = count_query_keys("token=a&eid=b&token=c&x=1");
        assert_eq!(c["token"], 2);
        assert_eq!(c["eid"], 1);
    }
}
