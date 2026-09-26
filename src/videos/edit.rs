//! sph edit：修改已发布视频的标题 / 描述 / 封面（UI 驱动，与 publish 同信任模型）。
//!
//! 平台机制（2026-09-26 spike 定案，真实账号校准）：
//! - 编辑入口是路由页 `/platform/post/coverEdit?objectId=<id>`（不是对话框）；
//! - 描述/短标题是**划词编辑**：选中差异区间 → 弹层 → 替换/删除，逐条计入预算
//!   （描述一次最多改 20 字、短标题 16 字；短标题总长须 6..=16 字；
//!   平台提示"仅支持修改一次，修改后不可撤回"）；
//! - 因此 --description/--title 的语义是"整体替换为目标文本"，但实现为
//!   最长公共前后缀 diff 后的一次区间替换；差异区间超预算 → 响亮拒绝（退出码 2）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::SharedWriter;
use crate::publish::mod_impl::OpenPageFn;
use crate::videos::list::find_by_id;
use crate::videos::page::{Selectors, VideoManagePage};
use crate::videos::{bootstrap, werr};

/// 平台单次修改预算（字）：描述。
pub(crate) const DESC_EDIT_BUDGET: usize = 20;
/// 平台单次修改预算（字）：短标题。
pub(crate) const TITLE_EDIT_BUDGET: usize = 16;
/// 短标题总长下限（平台校验）。
pub(crate) const TITLE_MIN_LEN: usize = 6;
/// 短标题总长上限（平台校验）。
pub(crate) const TITLE_MAX_LEN: usize = 16;

pub struct EditOptions {
    /// 目标视频 id（来自 sph list 输出）。
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub cover: Option<PathBuf>,
    /// 走完定位与表单填写但不提交。
    pub dry_run: bool,
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

/// edit 结果 DTO。
#[derive(Debug, Serialize)]
pub struct EditResult {
    pub account: String,
    pub id: String,
    /// 实际修改的字段（["title","description","cover"] 子集）。
    pub changed: Vec<String>,
    pub dry_run: bool,
    pub submitted: bool,
    /// 提交后重新拉列表对照的复检结果（dry-run 恒 false）。
    pub verified: bool,
}

/// 一次区间替换计划：把旧文本 [start, start+old_len) 替换为 new_text。
#[derive(Debug, PartialEq)]
pub(crate) struct SpanPlan {
    /// 起点（字符下标，非字节）。
    pub(crate) start: usize,
    /// 被替换区间长度（字符数；0 = 纯插入）。
    pub(crate) old_len: usize,
    pub(crate) new_text: String,
}

/// 最长公共前后缀 diff。old == new → None。
/// 平台划词编辑要求选中非空区间，纯插入（old_len=0）会扩成包含前一个字符
/// 的替换（start>0 时）或包含后一个字符的替换（start=0 时）。
pub(crate) fn diff_span(old: &str, new: &str) -> Option<SpanPlan> {
    if old == new {
        return None;
    }
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let mut prefix = 0;
    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix] == new_chars[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_chars.len() - prefix
        && suffix < new_chars.len() - prefix
        && old_chars[old_chars.len() - 1 - suffix] == new_chars[new_chars.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let mut start = prefix;
    let mut old_len = old_chars.len() - prefix - suffix;
    let mut new_text: String = new_chars[prefix..new_chars.len() - suffix].iter().collect();
    if old_len == 0 && !old_chars.is_empty() {
        if start > 0 {
            // 选中前一个字符，替换为「该字符 + 插入内容」
            start -= 1;
            old_len = 1;
            new_text = format!("{}{}", old_chars[start], new_text);
        } else {
            // 选中后一个字符，替换为「插入内容 + 该字符」
            old_len = 1;
            new_text = format!("{}{}", new_text, old_chars[0]);
        }
    }
    Some(SpanPlan { start, old_len, new_text })
}

/// 校验并规划一处文本修改。field 仅用于报错文案（"描述"/"短标题"）。
pub(crate) fn plan_text_edit(
    field: &str,
    old: &str,
    new: &str,
    budget: usize,
) -> Result<Option<SpanPlan>> {
    let Some(plan) = diff_span(old, new) else {
        return Ok(None);
    };
    let new_len = plan.new_text.chars().count();
    if plan.old_len > budget || new_len > budget {
        return Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!(
                "平台单次最多修改{budget}个字，本次{field}改动区间（旧 {} 字 → 新 {} 字）超限。\
                 请缩小改动范围（当前{field}与目标文本的最长公共前后缀之外的差异须 ≤{budget} 字）",
                plan.old_len, new_len
            ),
        ));
    }
    Ok(Some(plan))
}

/// 参数与本地状态校验（任何网络/浏览器活动之前）。
pub fn validate_edit(opts: &EditOptions) -> Result<()> {
    if opts.id.trim().is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "视频 id 不能为空（用 sph list 查看可用 id）",
        ));
    }
    if opts.title.is_none() && opts.description.is_none() && opts.cover.is_none() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "未指定任何待修改字段（--title / --description / --cover 至少一个）",
        ));
    }
    if let Some(t) = &opts.title {
        let len = t.chars().count();
        if !(TITLE_MIN_LEN..=TITLE_MAX_LEN).contains(&len) {
            return Err(AppError::fmt(
                Code::InvalidArgument,
                Stage::Arguments,
                format_args!("--title 长度须为 {TITLE_MIN_LEN}..={TITLE_MAX_LEN} 字（平台短标题限制），当前 {len} 字"),
            ));
        }
    }
    if let Some(cover) = &opts.cover {
        let meta = std::fs::metadata(cover).map_err(|_| {
            AppError::fmt(
                Code::InvalidArgument,
                Stage::Arguments,
                format_args!("封面文件不存在：{}", cover.display()),
            )
        })?;
        if !meta.is_file() || meta.len() == 0 {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "封面文件不可用",
            ));
        }
    }
    Ok(())
}

/// 执行修改。调用前必须先 validate_edit。
pub async fn run_edit(
    config_dir: &Path,
    account_name: &str,
    mut opts: EditOptions,
) -> Result<EditResult> {
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
    let result = run_edit_inner(&session, &opts).await;
    session.close().await;
    result.map(|mut r| {
        r.account = account_name.to_string();
        r
    })
}

async fn run_edit_inner(
    session: &crate::videos::SessionPage,
    opts: &EditOptions,
) -> Result<EditResult> {
    let stderr = &opts.stderr;
    let flow = VideoManagePage::new(&session.page, &session.selectors);

    // 1) 定位目标视频（找不到 → VideoUnavailable）；当前值用于 diff 规划
    werr(stderr, format_args!("正在定位视频 {}…\n", opts.id));
    let entry = find_by_id(session, &opts.id).await?;

    // 2) 规划文本修改（差异区间超平台预算在此响亮拒绝，浏览器已开但未做任何修改）
    let title_plan = match &opts.title {
        Some(new) => plan_text_edit("短标题", &entry.title, new, TITLE_EDIT_BUDGET)?,
        None => None,
    };
    let desc_plan = match &opts.description {
        Some(new) => plan_text_edit(
            "描述",
            entry.description.as_deref().unwrap_or_default(),
            new,
            DESC_EDIT_BUDGET,
        )?,
        None => None,
    };

    // 3) 直达编辑页（测试注入 navigate_url 时页面已就位，跳过导航）
    werr(stderr, format_args!("正在打开编辑页（{}）…\n", entry.title));
    flow.open_edit_page(&entry.id, opts.navigate_url.is_some())
        .await?;

    // 4) 逐字段修改（平台划词编辑链路）
    let mut changed: Vec<String> = Vec::new();
    if let Some(plan) = &title_plan {
        werr(stderr, format_args!("修改短标题（区间替换 {} 字 → {} 字）…\n", plan.old_len, plan.new_text.chars().count()));
        flow.apply_span_edit(
            session.selectors.edit_title_area.as_ref(),
            &entry.title,
            plan.start,
            plan.old_len,
            &plan.new_text,
        )
        .await?;
        changed.push("title".into());
    }
    if let Some(plan) = &desc_plan {
        werr(stderr, format_args!("修改描述（区间替换 {} 字 → {} 字）…\n", plan.old_len, plan.new_text.chars().count()));
        flow.apply_span_edit(
            session.selectors.edit_desc_area.as_ref(),
            entry.description.as_deref().unwrap_or_default(),
            plan.start,
            plan.old_len,
            &plan.new_text,
        )
        .await?;
        changed.push("description".into());
    }
    if let Some(cover) = &opts.cover {
        werr(stderr, format_args!("修改封面…\n"));
        flow.edit_set_cover(cover).await?;
        changed.push("cover".into());
    }

    if changed.is_empty() {
        werr(stderr, format_args!("所有字段与当前值相同，无需修改。\n"));
        return Ok(EditResult {
            account: String::new(),
            id: opts.id.clone(),
            changed,
            dry_run: opts.dry_run,
            submitted: false,
            verified: false,
        });
    }

    if opts.dry_run {
        werr(
            stderr,
            format_args!("dry-run 到此为止，未提交。可去掉 --dry-run 正式修改。\n"),
        );
        return Ok(EditResult {
            account: String::new(),
            id: opts.id.clone(),
            changed,
            dry_run: true,
            submitted: false,
            verified: false,
        });
    }

    // 5) 提交（完成 → 确认弹窗 → 成功/平台拒绝）
    werr(stderr, format_args!("提交修改…\n"));
    flow.submit_edit().await?;

    // 6) 复检：重新拉列表对照（失败不改写已提交事实）
    let verified = match find_by_id(session, &opts.id).await {
        Ok(after) => {
            let title_ok = opts.title.as_ref().map(|t| *t == after.title).unwrap_or(true);
            let desc_ok = opts
                .description
                .as_ref()
                .map(|d| Some(d) == after.description.as_ref())
                .unwrap_or(true);
            title_ok && desc_ok
        }
        Err(_) => false,
    };
    if !verified {
        werr(
            stderr,
            format_args!("警告：已提交但复检未能确认新值生效（平台可能有生效延迟），请用 sph list 人工确认。\n"),
        );
    }

    Ok(EditResult {
        account: String::new(),
        id: opts.id.clone(),
        changed,
        dry_run: false,
        submitted: true,
        verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_span_basic() {
        assert_eq!(diff_span("abc", "abc"), None);
        assert_eq!(
            diff_span("这集从零件出发", "这集从齿轮出发"),
            Some(SpanPlan { start: 3, old_len: 2, new_text: "齿轮".into() })
        );
        // 纯插入扩为含前字符的替换
        assert_eq!(
            diff_span("abc", "abXc"),
            Some(SpanPlan { start: 1, old_len: 1, new_text: "bX".into() })
        );
        // 纯删除
        assert_eq!(
            diff_span("abXc", "abc"),
            Some(SpanPlan { start: 2, old_len: 1, new_text: String::new() })
        );
        // 全替换
        assert_eq!(
            diff_span("aaa", "bbb"),
            Some(SpanPlan { start: 0, old_len: 3, new_text: "bbb".into() })
        );
        // 前后缀重叠不越界（old 是 new 的子串，首字符插入）
        assert_eq!(
            diff_span("aa", "aaaa"),
            Some(SpanPlan { start: 1, old_len: 1, new_text: "aaa".into() })
        );
        // 开头插入 → 扩为含首字符的替换
        assert_eq!(
            diff_span("bc", "Xbc"),
            Some(SpanPlan { start: 0, old_len: 1, new_text: "Xb".into() })
        );
    }

    #[test]
    fn plan_text_edit_budget() {
        // 预算内
        let p = plan_text_edit("描述", "abcde", "abXYe", 20).unwrap();
        assert_eq!(p, Some(SpanPlan { start: 2, old_len: 2, new_text: "XY".into() }));
        // 无差异 → None
        assert_eq!(plan_text_edit("描述", "abc", "abc", 20).unwrap(), None);
        // 旧区间超预算
        let old = "a".repeat(25);
        let err = plan_text_edit("描述", &old, "b", 20).unwrap_err();
        assert_eq!(err.code, Code::InvalidArgument);
        assert!(err.message.contains("20"));
        // 新区间超预算
        let new = "b".repeat(21);
        assert!(plan_text_edit("短标题", "", &new, 16).is_err());
    }
}
