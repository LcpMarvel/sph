//! 视频管理补丁文件：`~/.sph/patches/videos.json`
//!
//! 与 publish 补丁同构，但多一个 `api` 节——内部 CGI 端点与请求体模板
//! 本身就是最易漂移的"selector"，可热修而不发版：
//!
//! ```json
//! {
//!   "selectors": { "home_ready": ".brand-name" },
//!   "api": {
//!     "post_list_endpoint": "/cgi-bin/mmfinderassistant-bin/post/post_list",
//!     "post_list_body": "{\"pageSize\":__PAGE_SIZE__,\"page\":__PAGE__}",
//!     "page_size": 20
//!   }
//! }
//! ```
//!
//! 文件不存在 = 无补丁；JSON 损坏 / 未知字段 = 响亮报错。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::videos::page::Selectors;

/// 补丁文件内容（全部字段可选）。
#[derive(Debug, Default, Deserialize)]
pub struct VideosPatch {
    pub selectors: Option<BTreeMap<String, String>>,
    pub api: Option<ApiPatch>,
}

/// 内部 CGI 配置补丁（字段缺失时沿用内置默认；未知字段响亮拒绝）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiPatch {
    pub post_list_endpoint: Option<String>,
    pub post_list_body: Option<String>,
    pub page_size: Option<u32>,
    pub collection_list_endpoint: Option<String>,
    pub collection_list_body: Option<String>,
    pub collection_feed_endpoint: Option<String>,
    pub collection_feed_body: Option<String>,
}

/// 运行时实际使用的 API 配置（内置默认 + 补丁覆盖）。
#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub post_list_endpoint: Cow<'static, str>,
    /// 请求体模板：`__PAGE__`（从 1 开始）与 `__PAGE_SIZE__` 占位。
    pub post_list_body: Cow<'static, str>,
    pub page_size: u32,
    /// 合集清单 CGI。
    pub collection_list_endpoint: Cow<'static, str>,
    pub collection_list_body: Cow<'static, str>,
    /// 合集内视频清单 CGI；请求体模板多一个 `__COLLECTION_ID__` 占位。
    pub collection_feed_endpoint: Cow<'static, str>,
    pub collection_feed_body: Cow<'static, str>,
}

/// 内置默认（2026-09-26 spike 校准：分页参数是 currentPage，不是 page；
/// 响应 HTTP 201，data 含 list/totalCount/continueFlag）。
/// 合集接口（同日 sniff+collection 探针校准）：清单走 get_collection_list，
/// 成员走 get_collection_feed_list（post_list 的 collectionId 参数被静默忽略），
/// 分页参数 pageNum，响应 data.objectInfo[] = {feed, sortId}，total=collectionFeedCount。
/// 漂移时用补丁覆盖。
pub static DEFAULT_API: ApiConfig = ApiConfig {
    post_list_endpoint: Cow::Borrowed("/cgi-bin/mmfinderassistant-bin/post/post_list"),
    post_list_body: Cow::Borrowed(r#"{"pageSize":__PAGE_SIZE__,"currentPage":__PAGE__}"#),
    page_size: 20,
    collection_list_endpoint: Cow::Borrowed(
        "/cgi-bin/mmfinderassistant-bin/collection/get_collection_list",
    ),
    collection_list_body: Cow::Borrowed(
        r#"{"pageNum":__PAGE__,"pageSize":__PAGE_SIZE__,"collectionScene":0,"collectionBusinessType":0}"#,
    ),
    collection_feed_endpoint: Cow::Borrowed(
        "/cgi-bin/mmfinderassistant-bin/collection/get_collection_feed_list",
    ),
    collection_feed_body: Cow::Borrowed(
        r#"{"collectionId":"__COLLECTION_ID__","pageNum":__PAGE__,"pageSize":__PAGE_SIZE__}"#,
    ),
};

/// 补丁文件路径。
pub fn patch_path(config_dir: &Path) -> PathBuf {
    config_dir.join("patches").join("videos.json")
}

/// 读取补丁文件。不存在 → Ok(None)；损坏 → Err（响亮失败）。
pub fn load_patch(config_dir: &Path) -> Result<Option<VideosPatch>> {
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
    let patch: VideosPatch = serde_json::from_str(&raw).map_err(|_| {
        AppError::fmt(
            Code::InvalidArgument,
            Stage::SessionLoad,
            format_args!("补丁文件 {} 不是有效的 JSON", path.display()),
        )
    })?;
    Ok(Some(patch))
}

/// selector 补丁字段名清单（未知字段响亮拒绝）。
const KNOWN_SELECTOR_FIELDS: &[&str] = &[
    "home_ready",
    "login_indicator",
    "video_menu",
    "edit_page_ready",
    "edit_desc_area",
    "edit_title_area",
    "edit_pop_visible",
    "edit_pop_items",
    "edit_input",
    "edit_input_buttons",
    "edit_edited_badge",
    "edit_done_label",
    "edit_confirm_dialog",
    "edit_confirm_label",
    "edit_cover_button",
    "edit_cover_dialog",
    "edit_cover_file_input",
    "edit_cover_confirm_label",
    "edit_success_indicator",
    "edit_error_indicator",
];

/// 合并补丁到内置表（selector + api）。未知字段名 → 响亮报错。
pub fn merge(base: &'static Selectors, patch: &VideosPatch) -> Result<(Selectors, ApiConfig)> {
    let mut selectors = Selectors {
        home_ready: Cow::Borrowed(base.home_ready.as_ref()),
        login_indicator: Cow::Borrowed(base.login_indicator.as_ref()),
        video_menu: Cow::Borrowed(base.video_menu.as_ref()),
        edit_page_ready: Cow::Borrowed(base.edit_page_ready.as_ref()),
        edit_desc_area: Cow::Borrowed(base.edit_desc_area.as_ref()),
        edit_title_area: Cow::Borrowed(base.edit_title_area.as_ref()),
        edit_pop_visible: Cow::Borrowed(base.edit_pop_visible.as_ref()),
        edit_pop_items: Cow::Borrowed(base.edit_pop_items.as_ref()),
        edit_input: Cow::Borrowed(base.edit_input.as_ref()),
        edit_input_buttons: Cow::Borrowed(base.edit_input_buttons.as_ref()),
        edit_edited_badge: Cow::Borrowed(base.edit_edited_badge.as_ref()),
        edit_done_label: Cow::Borrowed(base.edit_done_label.as_ref()),
        edit_confirm_dialog: Cow::Borrowed(base.edit_confirm_dialog.as_ref()),
        edit_confirm_label: Cow::Borrowed(base.edit_confirm_label.as_ref()),
        edit_cover_button: Cow::Borrowed(base.edit_cover_button.as_ref()),
        edit_cover_dialog: Cow::Borrowed(base.edit_cover_dialog.as_ref()),
        edit_cover_file_input: Cow::Borrowed(base.edit_cover_file_input.as_ref()),
        edit_cover_confirm_label: Cow::Borrowed(base.edit_cover_confirm_label.as_ref()),
        edit_success_indicator: Cow::Borrowed(base.edit_success_indicator.as_ref()),
        edit_error_indicator: Cow::Borrowed(base.edit_error_indicator.as_ref()),
    };
    if let Some(map) = &patch.selectors {
        for (key, value) in map {
            if !KNOWN_SELECTOR_FIELDS.contains(&key.as_str()) {
                return Err(AppError::fmt(
                    Code::InvalidArgument,
                    Stage::SessionLoad,
                    format_args!(
                        "补丁包含未知 selector 字段：{key}（可用：{}）",
                        KNOWN_SELECTOR_FIELDS.join(", ")
                    ),
                ));
            }
            let target: &mut Cow<'static, str> = match key.as_str() {
                "home_ready" => &mut selectors.home_ready,
                "login_indicator" => &mut selectors.login_indicator,
                "video_menu" => &mut selectors.video_menu,
                "edit_page_ready" => &mut selectors.edit_page_ready,
                "edit_desc_area" => &mut selectors.edit_desc_area,
                "edit_title_area" => &mut selectors.edit_title_area,
                "edit_pop_visible" => &mut selectors.edit_pop_visible,
                "edit_pop_items" => &mut selectors.edit_pop_items,
                "edit_input" => &mut selectors.edit_input,
                "edit_input_buttons" => &mut selectors.edit_input_buttons,
                "edit_edited_badge" => &mut selectors.edit_edited_badge,
                "edit_done_label" => &mut selectors.edit_done_label,
                "edit_confirm_dialog" => &mut selectors.edit_confirm_dialog,
                "edit_confirm_label" => &mut selectors.edit_confirm_label,
                "edit_cover_button" => &mut selectors.edit_cover_button,
                "edit_cover_dialog" => &mut selectors.edit_cover_dialog,
                "edit_cover_file_input" => &mut selectors.edit_cover_file_input,
                "edit_cover_confirm_label" => &mut selectors.edit_cover_confirm_label,
                "edit_success_indicator" => &mut selectors.edit_success_indicator,
                "edit_error_indicator" => &mut selectors.edit_error_indicator,
                _ => unreachable!(),
            };
            *target = Cow::Owned(value.clone());
        }
    }

    let mut api = ApiConfig {
        post_list_endpoint: Cow::Borrowed(DEFAULT_API.post_list_endpoint.as_ref()),
        post_list_body: Cow::Borrowed(DEFAULT_API.post_list_body.as_ref()),
        page_size: DEFAULT_API.page_size,
        collection_list_endpoint: Cow::Borrowed(DEFAULT_API.collection_list_endpoint.as_ref()),
        collection_list_body: Cow::Borrowed(DEFAULT_API.collection_list_body.as_ref()),
        collection_feed_endpoint: Cow::Borrowed(DEFAULT_API.collection_feed_endpoint.as_ref()),
        collection_feed_body: Cow::Borrowed(DEFAULT_API.collection_feed_body.as_ref()),
    };
    if let Some(a) = &patch.api {
        fn check_endpoint(ep: &str) -> Result<()> {
            if ep.is_empty() || !ep.starts_with('/') {
                return Err(AppError::new(
                    Code::InvalidArgument,
                    Stage::SessionLoad,
                    "api.*_endpoint 必须是站内路径（以 / 开头）",
                ));
            }
            Ok(())
        }
        fn check_body(body: &str, extra_placeholder: Option<&str>) -> Result<()> {
            if !body.contains("__PAGE__")
                || extra_placeholder.is_some_and(|p| !body.contains(p))
            {
                return Err(AppError::fmt(
                    Code::InvalidArgument,
                    Stage::SessionLoad,
                    format_args!(
                        "api.*_body 必须包含 __PAGE__{} 占位",
                        extra_placeholder.map(|p| format!(" 与 {p}")).unwrap_or_default()
                    ),
                ));
            }
            Ok(())
        }
        if let Some(ep) = &a.post_list_endpoint {
            check_endpoint(ep)?;
            api.post_list_endpoint = Cow::Owned(ep.clone());
        }
        if let Some(body) = &a.post_list_body {
            check_body(body, None)?;
            api.post_list_body = Cow::Owned(body.clone());
        }
        if let Some(ep) = &a.collection_list_endpoint {
            check_endpoint(ep)?;
            api.collection_list_endpoint = Cow::Owned(ep.clone());
        }
        if let Some(body) = &a.collection_list_body {
            check_body(body, None)?;
            api.collection_list_body = Cow::Owned(body.clone());
        }
        if let Some(ep) = &a.collection_feed_endpoint {
            check_endpoint(ep)?;
            api.collection_feed_endpoint = Cow::Owned(ep.clone());
        }
        if let Some(body) = &a.collection_feed_body {
            check_body(body, Some("__COLLECTION_ID__"))?;
            api.collection_feed_body = Cow::Owned(body.clone());
        }
        if let Some(ps) = a.page_size {
            if ps == 0 || ps > 100 {
                return Err(AppError::new(
                    Code::InvalidArgument,
                    Stage::SessionLoad,
                    "api.page_size 必须在 1..=100",
                ));
            }
            api.page_size = ps;
        }
    }
    Ok((selectors, api))
}

/// 便捷函数：加载并合并（无补丁时原样返回内置表）。
pub fn load_and_merge(config_dir: &Path, base: &'static Selectors) -> Result<(Selectors, ApiConfig)> {
    match load_patch(config_dir)? {
        Some(patch) => merge(base, &patch),
        None => merge(base, &VideosPatch::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::videos::page::DEFAULT_SELECTORS;

    #[test]
    fn defaults_merge_clean() {
        let (sel, api) = merge(&DEFAULT_SELECTORS, &VideosPatch::default()).unwrap();
        assert_eq!(sel.home_ready, DEFAULT_SELECTORS.home_ready);
        assert!(api.post_list_endpoint.starts_with("/cgi-bin/"));
        assert_eq!(api.page_size, 20);
    }

    #[test]
    fn selector_override_and_unknown_field() {
        let patch: VideosPatch =
            serde_json::from_str(r#"{"selectors": {"home_ready": ".x"}}"#).unwrap();
        let (sel, _) = merge(&DEFAULT_SELECTORS, &patch).unwrap();
        assert_eq!(sel.home_ready, ".x");

        let bad: VideosPatch =
            serde_json::from_str(r#"{"selectors": {"home_redy": ".x"}}"#).unwrap();
        assert!(merge(&DEFAULT_SELECTORS, &bad).is_err());
    }

    #[test]
    fn api_override_validation() {
        let patch: VideosPatch = serde_json::from_str(
            r#"{"api": {"post_list_endpoint": "/cgi-bin/x/y", "page_size": 50}}"#,
        )
        .unwrap();
        let (_, api) = merge(&DEFAULT_SELECTORS, &patch).unwrap();
        assert_eq!(api.post_list_endpoint, "/cgi-bin/x/y");
        assert_eq!(api.page_size, 50);

        let bad: VideosPatch =
            serde_json::from_str(r#"{"api": {"post_list_body": "{\"a\":1}"}}"#).unwrap();
        assert!(merge(&DEFAULT_SELECTORS, &bad).is_err()); // 缺 __PAGE__ 占位
    }

    #[test]
    fn collection_api_override_validation() {
        let patch: VideosPatch = serde_json::from_str(
            r#"{"api": {
                "collection_feed_endpoint": "/cgi-bin/c/f",
                "collection_feed_body": "{\"collectionId\":\"__COLLECTION_ID__\",\"pageNum\":__PAGE__}"
            }}"#,
        )
        .unwrap();
        let (_, api) = merge(&DEFAULT_SELECTORS, &patch).unwrap();
        assert_eq!(api.collection_feed_endpoint, "/cgi-bin/c/f");

        // 缺 __COLLECTION_ID__ 占位 → 响亮拒绝
        let bad: VideosPatch = serde_json::from_str(
            r#"{"api": {"collection_feed_body": "{\"pageNum\":__PAGE__}"}}"#,
        )
        .unwrap();
        assert!(merge(&DEFAULT_SELECTORS, &bad).is_err());

        // 未知 api 字段 → serde deny_unknown_fields 拒绝
        assert!(serde_json::from_str::<VideosPatch>(
            r#"{"api": {"collection_feed_endpoin": "/x"}}"#
        )
        .is_err());
    }

    #[test]
    fn missing_patch_file_is_none() {
        let dir = std::env::temp_dir().join(format!("sph-vpatch-none-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        assert!(load_patch(&dir).unwrap().is_none());
    }
}
