//! 页面状态快照：失败时的现场采集（PRD §7.2 的 page_state 层）。
//!
//! 蒸馏格式借鉴 browser-use 的思路：把页面压成带序号的可交互元素清单，
//! 人和模型都能直接读懂。快照连同截图一起落盘 `~/.sph/crashes/<时间戳>/`，
//! 作为恢复层的输入与事后审计素材。

use std::path::Path;

use chromiumoxide::Page;
use serde::{Deserialize, Serialize};

use crate::apperr::{AppError, Code, Result, Stage};

/// 单个可交互元素的蒸馏描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElementBrief {
    /// 展示序号（LLM/人工引用用）。
    pub index: usize,
    pub tag: String,
    /// 可见文本（截断）。
    pub text: String,
    /// 属性线索：id/class/placeholder/role（截断，不含值秘密）。
    pub attrs: String,
}

/// 一次失败现场的完整快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub captured_at: String,
    /// 失败阶段（upload / metadata / ...）。
    pub stage: String,
    /// 已尝试的内置 selector。
    pub attempted_selector: String,
    /// 蒸馏出的可交互元素（带序号）。
    pub elements: Vec<ElementBrief>,
}

const DISTILL_JS: &str = r#"
(function(){
  const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
  roots.push(document);
  const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
  const brief = (el, index) => {
    const parts = [];
    const id = el.getAttribute && el.getAttribute('id');
    if (id) parts.push('#' + id);
    const cls = (typeof el.className === 'string' ? el.className : '').trim().split(/\s+/).slice(0, 2).map(c => '.' + c).join('');
    if (cls) parts.push(cls);
    const ph = el.getAttribute && el.getAttribute('placeholder');
    if (ph) parts.push('placeholder=' + ph.slice(0, 20));
    const role = el.getAttribute && el.getAttribute('role');
    if (role) parts.push('role=' + role);
    return {
      index,
      tag: el.tagName.toLowerCase(),
      text: (el.innerText || el.value || '').trim().slice(0, 24),
      attrs: parts.join(' ').slice(0, 60),
    };
  };
  const out = [];
  let i = 0;
  const seen = new Set();
  roots.forEach(root => {
    root.querySelectorAll('button, a, input, textarea, [contenteditable=true], [role=button], select').forEach(el => {
      if (!vis(el) || seen.has(el)) return;
      seen.add(el);
      out.push(brief(el, i++));
    });
  });
  return JSON.stringify({url: location.href, title: document.title, elements: out.slice(0, 120)});
})()
"#;

/// 采集当前页面状态。
pub async fn capture(page: &Page, stage: Stage, attempted_selector: &str) -> Result<PageSnapshot> {
    let raw = page.evaluate(DISTILL_JS).await.map_err(|e| {
        AppError::fmt(
            Code::InternalError,
            stage,
            format_args!("页面快照失败: {e}"),
        )
    })?;
    let value = raw.value().cloned().unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_value(value).unwrap_or_default();
    let elements: Vec<ElementBrief> = parsed
        .get("elements")
        .and_then(|e| serde_json::from_value(e.clone()).ok())
        .unwrap_or_default();
    let now = time::OffsetDateTime::now_utc();
    Ok(PageSnapshot {
        url: parsed
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        title: parsed
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        captured_at: now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        stage: stage.as_str().to_string(),
        attempted_selector: attempted_selector.to_string(),
        elements,
    })
}

impl PageSnapshot {
    /// 渲染为给人/模型读的文本形式（每个元素一行）。
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "url: {}\ntitle: {}\nstage: {}\nselector: {}\ncaptured: {}\n",
            self.url, self.title, self.stage, self.attempted_selector, self.captured_at
        );
        for el in &self.elements {
            out.push_str(&format!(
                "[{}] <{}> \"{}\" {}\n",
                el.index, el.tag, el.text, el.attrs
            ));
        }
        out
    }

    /// 渲染为紧凑 JSON。
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// 保存失败现场：快照 JSON + 可交互元素文本 + 页面截图。
/// 返回现场目录路径（写入错误不覆盖原始错误，仅警告）。
pub async fn save_scene(
    page: &Page,
    snapshot: &PageSnapshot,
    crashes_dir: &Path,
) -> Option<String> {
    use rand::Rng;
    let dir = crashes_dir.join(format!(
        "{}-{:04x}",
        snapshot.captured_at.replace([':', 'T', 'Z', '-'], ""),
        rand::thread_rng().gen::<u16>()
    ));
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::write(dir.join("snapshot.json"), snapshot.to_json()).ok()?;
    std::fs::write(dir.join("page.txt"), snapshot.to_text()).ok()?;
    if let Ok(bytes) = page
        .screenshot(chromiumoxide::page::ScreenshotParams::default())
        .await
    {
        let _ = std::fs::write(dir.join("screenshot.png"), bytes);
    }
    Some(dir.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_text_renders_elements() {
        let snap = PageSnapshot {
            url: "https://channels.weixin.qq.com/platform/post/create".into(),
            title: "视频号助手".into(),
            captured_at: "2026-09-24T12:00:00Z".into(),
            stage: "upload".into(),
            attempted_selector: "input[type=file]".into(),
            elements: vec![
                ElementBrief {
                    index: 0,
                    tag: "button".into(),
                    text: "发表视频".into(),
                    attrs: ".weui-desktop-btn".into(),
                },
                ElementBrief {
                    index: 1,
                    tag: "input".into(),
                    text: String::new(),
                    attrs: "#video-file".into(),
                },
            ],
        };
        let text = snap.to_text();
        assert!(text.contains("[0] <button> \"发表视频\""));
        assert!(text.contains("stage: upload"));
        assert!(text.contains("input[type=file]"));
        let json = snap.to_json();
        assert!(json.contains("\"stage\": \"upload\""));
    }
}
