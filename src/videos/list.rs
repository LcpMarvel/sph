//! sph list：拉取已发布视频列表（页面上下文内部 CGI + 可选合集过滤）。

use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::SharedWriter;
use crate::publish::mod_impl::OpenPageFn;
use crate::videos::api::{fetch_all, fetch_collection_feeds, fetch_collections, VideoEntry};
use crate::videos::page::Selectors;
use crate::videos::{bootstrap, werr};

/// 默认拉取条数与上限。
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 200;

pub struct ListOptions {
    /// 按合集名过滤（精确匹配；服务端成员关系，见 api::fetch_collection_feeds）。
    pub collection: Option<String>,
    pub limit: usize,
    pub headed: bool,
    pub timeout: Duration,
    pub cancel: crate::http::CancelToken,
    pub stderr: SharedWriter,
    /// 测试注入：页面工厂。
    pub open_page: Option<OpenPageFn>,
    /// 测试注入：导航 URL 覆盖。
    pub navigate_url: Option<String>,
    /// selector 覆盖（测试）。
    pub selectors: Option<Selectors>,
}

/// list 结果 DTO。
#[derive(Debug, Serialize)]
pub struct ListResult {
    pub account: String,
    pub count: usize,
    pub videos: Vec<VideoEntry>,
    /// 固定 false：合集过滤自 2.1.0 起走服务端成员接口（保留字段仅为 JSON 契约稳定）。
    pub client_side_collection_filter: bool,
}

pub async fn run_list(
    config_dir: &Path,
    account_name: &str,
    mut opts: ListOptions,
) -> Result<ListResult> {
    let limit = opts.limit.clamp(1, MAX_LIMIT);
    let session = bootstrap(
        config_dir,
        account_name,
        opts.headed,
        opts.open_page.take(),
        opts.navigate_url.as_deref(),
        Duration::from_secs(60),
        &opts.cancel,
        &opts.stderr,
        opts.selectors.clone(),
    )
    .await?;
    let stderr = opts.stderr.clone();
    werr(&stderr, format_args!("正在拉取已发布视频列表…\n"));
    let api = session.api.clone();
    let result = async {
        let wujie = crate::browser::wujie::Wujie::new(&session.page);
        let t = Duration::from_secs(30);
        let videos = if let Some(want) = &opts.collection {
            // 合集过滤 = 服务端成员关系（spike 2026-09-26：post_list 不支持
            // 合集参数，成员走 get_collection_feed_list）。名称 → id 精确匹配，
            // 找不到时列出可用合集名帮助用户纠正。
            let want = want.trim();
            let collections = fetch_collections(&wujie, &api, t).await?;
            let col = collections.iter().find(|c| c.name == want).ok_or_else(|| {
                let names: Vec<&str> = collections.iter().map(|c| c.name.as_str()).collect();
                AppError::fmt(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    format_args!("合集「{want}」不存在（可用：{}）", names.join("、")),
                )
            })?;
            werr(&stderr, format_args!("按合集「{}」过滤…\n", col.name));
            let mut videos = fetch_collection_feeds(&wujie, &api, &col.id, limit, t).await?;
            for v in &mut videos {
                v.collection = Some(col.name.clone());
            }
            videos
        } else {
            fetch_all(&wujie, &api, limit, t).await?
        };
        Ok::<Vec<VideoEntry>, AppError>(videos)
    }
    .await;
    let videos = match result {
        Ok(v) => v,
        Err(e) => {
            session.close().await;
            return Err(e);
        }
    };
    session.close().await;
    let count = videos.len();
    werr(&stderr, format_args!("共 {count} 条。\n"));
    Ok(ListResult {
        account: account_name.to_string(),
        count,
        videos,
        client_side_collection_filter: false,
    })
}

/// 供 edit 复用的按 id 定位。
pub(crate) async fn find_by_id(
    session: &crate::videos::SessionPage,
    id: &str,
) -> Result<VideoEntry> {
    let wujie = crate::browser::wujie::Wujie::new(&session.page);
    let videos = fetch_all(&wujie, &session.api, MAX_LIMIT, Duration::from_secs(30)).await?;
    videos.into_iter().find(|v| v.id == id).ok_or_else(|| {
        AppError::fmt(
            Code::VideoUnavailable,
            Stage::Arguments,
            format_args!("视频 {id} 不存在或不在已发布列表中（用 sph list 查看可用 id）"),
        )
    })
}
