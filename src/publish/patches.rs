//! selector 补丁机制（M3 自愈闭环的第一块）：把修复单元从"改源码发版"
//! 变成"运行时 JSON 覆盖"。
//!
//! 补丁文件：`~/.sph/patches/publish.json`
//! ```json
//! { "selectors": { "title_input": "input[placeholder*='新标题']" } }
//! ```
//! 只覆盖列出的字段，其余沿用内置默认。文件不存在 = 无补丁；
//! JSON 损坏 = 响亮报错（绝不静默跳过——坏补丁比没有补丁更危险）。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::publish::page::Selectors;

/// 补丁文件内容（全部字段可选）。
#[derive(Debug, Default, Deserialize)]
pub struct PublishPatch {
    pub selectors: Option<BTreeMap<String, String>>,
}

/// 补丁文件路径。
pub fn patch_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("patches").join("publish.json")
}

/// 读取补丁文件。不存在 → Ok(None)；损坏 → Err（响亮失败）。
pub fn load_patch(config_dir: &Path) -> Result<Option<PublishPatch>> {
    let path = patch_path(config_dir);
    let raw = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::SessionLoad,
                format_args!("无法读取补丁文件 {}: {e}", path.display()),
            ))
        }
        Ok(raw) => raw,
    };
    let patch: PublishPatch = serde_json::from_str(&raw).map_err(|_| {
        AppError::fmt(
            Code::InvalidArgument,
            Stage::SessionLoad,
            format_args!("补丁文件 {} 不是有效的 JSON", path.display()),
        )
    })?;
    Ok(Some(patch))
}

/// 补丁字段名清单（用于拒绝拼错的未知字段——响亮失败）。
const KNOWN_FIELDS: &[&str] = &[
    "home_ready",
    "login_indicator",
    "publish_entry",
    "video_menu",
    "video_file_input",
    "upload_done_indicator",
    "title_input",
    "description_editor",
    "topic_prefix",
    "cover_file_input",
    "original_declaration_checkbox",
    "original_declaration_label",
    "submit_button",
    "publish_success_indicator",
    "publish_error_indicator",
];

/// 把补丁合并到内置 selector 上，得到本次运行实际使用的表。
/// 未知字段名 → 响亮报错（打错一个字母静默生效是调试噩梦）。
pub fn merge(base: &'static Selectors, patch: &PublishPatch) -> Result<Selectors> {
    let mut out = Selectors {
        home_ready: Cow::Borrowed(base.home_ready.as_ref()),
        login_indicator: Cow::Borrowed(base.login_indicator.as_ref()),
        publish_entry: Cow::Borrowed(base.publish_entry.as_ref()),
        video_menu: Cow::Borrowed(base.video_menu.as_ref()),
        video_file_input: Cow::Borrowed(base.video_file_input.as_ref()),
        upload_done_indicator: Cow::Borrowed(base.upload_done_indicator.as_ref()),
        title_input: Cow::Borrowed(base.title_input.as_ref()),
        description_editor: Cow::Borrowed(base.description_editor.as_ref()),
        topic_prefix: Cow::Borrowed(base.topic_prefix.as_ref()),
        cover_file_input: Cow::Borrowed(base.cover_file_input.as_ref()),
        original_declaration_checkbox: Cow::Borrowed(base.original_declaration_checkbox.as_ref()),
        original_declaration_label: Cow::Borrowed(base.original_declaration_label.as_ref()),
        submit_button: Cow::Borrowed(base.submit_button.as_ref()),
        publish_success_indicator: Cow::Borrowed(base.publish_success_indicator.as_ref()),
        publish_error_indicator: Cow::Borrowed(base.publish_error_indicator.as_ref()),
    };
    let Some(selectors) = &patch.selectors else {
        return Ok(out);
    };
    for (key, value) in selectors {
        if !KNOWN_FIELDS.contains(&key.as_str()) {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::SessionLoad,
                format_args!(
                    "补丁包含未知 selector 字段：{key}（可用：{}）",
                    KNOWN_FIELDS.join(", ")
                ),
            ));
        }
        let target: &mut Cow<'static, str> = match key.as_str() {
            "home_ready" => &mut out.home_ready,
            "login_indicator" => &mut out.login_indicator,
            "publish_entry" => &mut out.publish_entry,
            "video_menu" => &mut out.video_menu,
            "video_file_input" => &mut out.video_file_input,
            "upload_done_indicator" => &mut out.upload_done_indicator,
            "title_input" => &mut out.title_input,
            "description_editor" => &mut out.description_editor,
            "topic_prefix" => &mut out.topic_prefix,
            "cover_file_input" => &mut out.cover_file_input,
            "original_declaration_checkbox" => &mut out.original_declaration_checkbox,
            "original_declaration_label" => &mut out.original_declaration_label,
            "submit_button" => &mut out.submit_button,
            "publish_success_indicator" => &mut out.publish_success_indicator,
            "publish_error_indicator" => &mut out.publish_error_indicator,
            _ => unreachable!(),
        };
        *target = Cow::Owned(value.clone());
    }
    Ok(out)
}

/// 便捷函数：加载并合并（无补丁时原样返回内置表）。
pub fn load_and_merge(config_dir: &Path, base: &'static Selectors) -> Result<Selectors> {
    match load_patch(config_dir)? {
        Some(patch) => merge(base, &patch),
        None => Ok(Selectors {
            home_ready: Cow::Borrowed(base.home_ready.as_ref()),
            login_indicator: Cow::Borrowed(base.login_indicator.as_ref()),
            publish_entry: Cow::Borrowed(base.publish_entry.as_ref()),
            video_menu: Cow::Borrowed(base.video_menu.as_ref()),
            video_file_input: Cow::Borrowed(base.video_file_input.as_ref()),
            upload_done_indicator: Cow::Borrowed(base.upload_done_indicator.as_ref()),
            title_input: Cow::Borrowed(base.title_input.as_ref()),
            description_editor: Cow::Borrowed(base.description_editor.as_ref()),
            topic_prefix: Cow::Borrowed(base.topic_prefix.as_ref()),
            cover_file_input: Cow::Borrowed(base.cover_file_input.as_ref()),
            original_declaration_checkbox: Cow::Borrowed(
                base.original_declaration_checkbox.as_ref(),
            ),
            original_declaration_label: Cow::Borrowed(base.original_declaration_label.as_ref()),
            submit_button: Cow::Borrowed(base.submit_button.as_ref()),
            publish_success_indicator: Cow::Borrowed(base.publish_success_indicator.as_ref()),
            publish_error_indicator: Cow::Borrowed(base.publish_error_indicator.as_ref()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::page::DEFAULT_SELECTORS;

    fn patch_json(s: &str) -> PublishPatch {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn merge_overrides_only_listed_fields() {
        let patch = patch_json(r#"{"selectors": {"title_input": "input#new-title"}}"#);
        let merged = merge(&DEFAULT_SELECTORS, &patch).unwrap();
        assert_eq!(merged.title_input, "input#new-title");
        assert_eq!(merged.submit_button, DEFAULT_SELECTORS.submit_button);
        assert_eq!(merged.home_ready, DEFAULT_SELECTORS.home_ready);
    }

    #[test]
    fn empty_patch_keeps_defaults() {
        let merged = merge(&DEFAULT_SELECTORS, &PublishPatch::default()).unwrap();
        assert_eq!(merged.title_input, DEFAULT_SELECTORS.title_input);
    }

    #[test]
    fn unknown_field_is_loud_error() {
        let patch = patch_json(r#"{"selectors": {"titel_input": "x"}}"#); // 拼错
        let err = merge(&DEFAULT_SELECTORS, &patch).unwrap_err();
        assert_eq!(err.code, Code::InvalidArgument);
        assert!(err.message.contains("titel_input"));
    }

    #[test]
    fn corrupt_patch_file_is_loud_error() {
        let dir = std::env::temp_dir().join(format!("sph-patch-test-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        std::fs::create_dir_all(dir.join("patches")).unwrap();
        std::fs::write(dir.join("patches").join("publish.json"), b"{not json").unwrap();
        let err = load_patch(&dir).unwrap_err();
        assert_eq!(err.code, Code::InvalidArgument);
    }

    #[test]
    fn missing_patch_is_none() {
        let dir = std::env::temp_dir().join(format!("sph-patch-none-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        assert!(load_patch(&dir).unwrap().is_none());
    }
}
