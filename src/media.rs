//! 解析结果模型与其安全 DTO。内部的 ResolvedVideo 携带带签名的媒体 URL，
//! 绝不可被直接序列化进命令输出。

use serde::Serialize;
use sha2::{Digest, Sha256};

/// 两步解析链的结果。media_url 已签名，因此不参与任何 JSON 序列化。
#[derive(Debug, Clone, Default)]
pub struct ResolvedVideo {
    pub share_url: String, // 归一化后的分享链接（内部使用）
    pub local_id: String,  // 分享链接 SHA-256 的 12 位十六进制前缀
    pub title: String,
    pub author: String,
    pub media_url: String,    // 签名 URL —— 永不序列化
    pub media_source: String, // h264VideoInfo | h265VideoInfo | videoUrl
    pub codec_hint: String,   // h264 | h265 | ""
}

/// `inspect --json` 打印的安全 DTO。
#[derive(Debug, Serialize)]
pub struct InspectResult {
    #[serde(rename = "local_id")]
    pub local_id: String,
    pub title: String,
    pub author: String,
    #[serde(rename = "media_source")]
    pub media_source: String,
    #[serde(rename = "codec_hint")]
    pub codec_hint: String,
}

impl ResolvedVideo {
    /// 构建安全 DTO。
    pub fn inspect(&self) -> InspectResult {
        InspectResult {
            local_id: self.local_id.clone(),
            title: sanitize_text(&self.title),
            author: sanitize_text(&self.author),
            media_source: self.media_source.clone(),
            codec_hint: self.codec_hint.clone(),
        }
    }
}

/// 计算稳定的本地标识：归一化分享链接字节的 SHA-256 前 12 个十六进制字符。
/// 它不是腾讯视频 ID。
pub fn local_id(normalized_share_url: &str) -> String {
    let sum = Sha256::digest(normalized_share_url.as_bytes());
    let mut out = String::with_capacity(12);
    for b in &sum[..6] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 外部文本到达 stdout 前剥掉终端控制字符。
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for r in s.chars() {
        match r {
            '\n' | '\t' => out.push(' '),
            r if (r as u32) < 0x20 || r == '\u{7f}' => {}
            r => out.push(r),
        }
    }
    out
}

/// 生成文件名的字节上限。
pub const MAX_BASENAME_BYTES: usize = 200;

/// 将外部标题净化为可安全用于文件名的形式：路径分隔符、控制字符和
/// : * ? " < > | 被移除，空白折叠为单个空格，结尾的空格/点被剥离，
/// "." 与 ".." 被拒绝，结果按 UTF-8 字节截断到上限且不拆断字符。
pub fn safe_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_space = false;
    for r in title.chars() {
        match r {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => {}
            r if r.is_whitespace() => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            r if (r as u32) < 0x20 || r == '\u{7f}' => {}
            r => {
                out.push(r);
                last_space = false;
            }
        }
    }
    let mut out = out.trim_end_matches([' ', '.']).to_string();
    if out.is_empty() || out == "." || out == ".." {
        return "video".into();
    }
    // 按字节截断，不拆断 UTF-8 字符：反复移除越过上限的最后一个字符
    while out.len() > MAX_BASENAME_BYTES {
        match out
            .char_indices()
            .take_while(|(i, _)| *i < MAX_BASENAME_BYTES)
            .last()
        {
            Some((i, _)) => out.truncate(i),
            None => return "video".into(),
        }
    }
    let out = out.trim_end_matches([' ', '.']).to_string();
    if out.is_empty() {
        "video".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_id_stable_and_12_hex() {
        let a = local_id("https://weixin.qq.com/sph/AogyNMyA7L");
        let b = local_id("https://weixin.qq.com/sph/AogyNMyA7L");
        let c = local_id("https://weixin.qq.com/sph/Other1234");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 12);
        assert!(a
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_alphabetic()
                || ch.is_ascii_digit()
                || ('a'..='f').contains(&ch)));
    }

    #[test]
    fn safe_title_cases() {
        let cases: Vec<(&str, &str)> = vec![
            ("正常标题", "正常标题"),
            ("a/b\\c:d*e?f\"g<h>i|j", "abcdefghij"),
            ("控制\u{0}字符\u{7f}过滤", "控制字符过滤"),
            ("多   个 空白\t折叠", "多 个 空白 折叠"),
            ("结尾空格和点... ", "结尾空格和点"),
            (".", "video"),
            ("..", "video"),
            ("", "video"),
            ("   ", "video"),
        ];
        for (input, want) in cases {
            assert_eq!(safe_title(input), want, "input {input:?}");
        }
    }

    #[test]
    fn safe_title_byte_cap() {
        let long = "字".repeat(150); // 450 字节
        let got = safe_title(&long);
        assert!(got.len() <= MAX_BASENAME_BYTES);
        assert!(got.chars().all(|r| r != '\u{FFFD}'));
    }

    #[test]
    fn sanitize_text_strips_control_chars() {
        let out = sanitize_text("标题\u{1b}[31m红\u{7}音\n换行\t制表");
        assert!(!out.contains(['\u{1b}', '\u{7}', '\u{7f}', '\n']));
        assert!(out.contains("标题"));
        assert!(out.contains("制表"));
    }

    #[test]
    fn inspect_dto_does_not_expose_internals() {
        let v = ResolvedVideo {
            share_url: "https://weixin.qq.com/sph/X".into(),
            media_url: "https://cdn.example/v.mp4?sig=SECRET".into(),
            ..Default::default()
        };
        let dto = v.inspect();
        assert!(!dto.title.contains("SECRET"));
        assert!(!dto.author.contains("SECRET"));
        let json = serde_json::to_string(&dto).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("media_url"));
    }
}
