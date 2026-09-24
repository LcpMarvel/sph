//! 发布流水线实现：加载会话 → 导航 → 上传 → 元信息 → 封面 → 声明 → 提交。
//!
//! 固定流程，零 LLM。任何一步失败即报错退出（M3 在此挂恢复层）。
//! dry-run 走完除提交外的全部步骤。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::{self, fetch, SharedWriter};
use crate::publish::page::{PublishPage, Selectors, DEFAULT_SELECTORS};
use crate::publish::patches;
use crate::publish::recovery::{self, NullBackend, RecoveryBackend};
use crate::session::{self, AccountDir};

const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(60);
const HOME_URL: &str = "https://channels.weixin.qq.com/platform";

/// 一次发布的配置。
pub struct Options {
    pub video: PathBuf,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub cover: Option<PathBuf>,
    pub dry_run: bool,
    pub headed: bool,
    pub timeout: Duration,
    pub cancel: crate::http::CancelToken,
    pub stderr: SharedWriter,
    /// 测试注入：页面工厂（真实实现启动 chromiumoxide，返回 page 与持有的 browser）。
    pub open_page: Option<OpenPageFn>,
    /// 选择器覆盖（测试 / 未来的补丁机制）。
    pub selectors: Option<Selectors>,
    /// 导航 URL 覆盖（测试注入 fixture；生产恒为 HOME_URL）。
    pub navigate_url: Option<String>,
    /// 单步超时（测试可注入小值；默认 60s）。
    pub step_timeout: Duration,
    /// 定时发表时间（本地时区；None = 立即发表）。
    pub schedule_at: Option<time::OffsetDateTime>,
}

/// 页面工厂类型别名（测试注入假实现）。
pub type OpenPageFn = Arc<
    dyn Fn(&Path, bool) -> futures::future::BoxFuture<'static, Result<OpenedPage>> + Send + Sync,
>;

/// 打开完成的页面（browser 句柄保持存活）。
pub struct OpenedPage {
    #[allow(dead_code)]
    pub browser: Option<chromiumoxide::browser::Browser>,
    pub page: chromiumoxide::Page,
}

/// 发布成功的安全 DTO。
#[derive(Debug, Serialize)]
pub struct PublishResult {
    pub account: String,
    pub video: String,
    pub title: String,
    #[serde(rename = "dry_run")]
    pub dry_run: bool,
    pub submitted: bool,
    #[serde(rename = "scheduled_at", skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<String>,
}

fn werr(stderr: &SharedWriter, args: std::fmt::Arguments) {
    if let Ok(mut g) = stderr.lock() {
        let _ = g.write_fmt(args);
        let _ = g.flush();
    }
}

/// 参数与本地状态校验（任何网络/浏览器活动之前）。
pub fn validate(opts: &Options) -> Result<()> {
    let video = &opts.video;
    let meta = std::fs::metadata(video).map_err(|_| {
        AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("视频文件不存在：{}", video.display()),
        )
    })?;
    if !meta.is_file() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "视频路径不是普通文件",
        ));
    }
    if meta.len() == 0 {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "视频文件为空",
        ));
    }
    if opts.title.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "标题不能为空（使用 --title 指定）",
        ));
    }
    if let Some(cover) = &opts.cover {
        let cmeta = std::fs::metadata(cover).map_err(|_| {
            AppError::fmt(
                Code::InvalidArgument,
                Stage::Arguments,
                format_args!("封面文件不存在：{}", cover.display()),
            )
        })?;
        if !cmeta.is_file() || cmeta.len() == 0 {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "封面文件不可用",
            ));
        }
    }
    Ok(())
}

/// 执行发布。调用前必须先 validate。
pub async fn run(
    config_dir: &Path,
    account_name: &str,
    mut opts: Options,
) -> Result<PublishResult> {
    let account_dir = AccountDir::new(&session::accounts_root(config_dir), account_name);
    // 会话加载：profile 存在即视为有会话（有效性在导航后复核）
    if !account_dir.profile_exists() {
        return Err(AppError::fmt(
            Code::SessionExpired,
            Stage::SessionLoad,
            format_args!("账号 {account_name} 没有已保存的助手会话，请执行 sph login。"),
        ));
    }
    let profile_dir = account_dir.profile_dir();
    // selector = 内置默认 + ~/.sph/patches/publish.json 覆盖（M3 补丁机制）
    let selectors = match &opts.selectors {
        Some(custom) => custom.clone(),
        None => patches::load_and_merge(config_dir, &DEFAULT_SELECTORS)?,
    };

    let stderr = opts.stderr.clone();
    werr(
        &stderr,
        format_args!("正在加载视频号助手会话（账号: {account_name}）…\n"),
    );

    let opened: OpenedPage = match opts.open_page.take() {
        Some(open) => open(&profile_dir, opts.headed).await?,
        None => {
            let chrome = fetch::ensure_chromium(stderr.clone()).await?;
            let (b, p) = browser::launch(&chrome, &profile_dir, opts.headed).await?;
            OpenedPage {
                browser: Some(b),
                page: p,
            }
        }
    };
    let mut browser = opened.browser;
    let page = opened.page;

    let result = run_inner(config_dir, account_name, &page, &selectors, &opts).await;
    // 优雅关闭：强杀进程会让持久 profile 里刚产生的会话变更（cookie 等）
    // 来不及落盘，下次启动会话丢失；CDP Browser.close 是干净退出。
    if let Some(b) = browser.as_mut() {
        let _ = b.close().await;
    }
    drop(browser);
    tokio::time::sleep(Duration::from_millis(500)).await;
    result
}

async fn run_inner(
    config_dir: &Path,
    account_name: &str,
    page: &chromiumoxide::Page,
    selectors: &Selectors,
    opts: &Options,
) -> Result<PublishResult> {
    let backend: Arc<dyn RecoveryBackend> = Arc::new(NullBackend);
    run_inner_with_backend(config_dir, account_name, page, selectors, opts, &backend).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_inner_with_backend(
    config_dir: &Path,
    account_name: &str,
    page: &chromiumoxide::Page,
    selectors: &Selectors,
    opts: &Options,
    backend: &Arc<dyn RecoveryBackend>,
) -> Result<PublishResult> {
    let stderr = &opts.stderr;
    // 带恢复重试的步骤执行器：每步失败 → 快照+现场 → 后端决策 → 执行+重试一次
    macro_rules! step {
        ($stage:expr, $sel:expr, $fut:expr) => {
            recovery::recover_step(config_dir, page, backend, $stage, $sel, || $fut).await?
        };
    }
    // 直达创建页会被平台重定向回主页（wujie 路由守卫），
    // 正确路径：主页就绪 → 点掉可能的引导弹窗 → 点击"发表视频"入口。
    let home_url = opts.navigate_url.as_deref().unwrap_or(HOME_URL);
    navigate(page, home_url, &opts.cancel).await?;
    let flow = PublishPage::new(page, selectors);
    flow.require_logged_in().await?;
    step!(
        Stage::Navigate,
        selectors.home_ready.as_ref(),
        flow.wait_home_ready(opts.step_timeout)
    );
    // 引导弹窗（切换视频号/我知道了等）间歇出现，全部点掉
    for _ in 0..3 {
        if !flow.dismiss_dialogs().await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    step!(
        Stage::Navigate,
        selectors.video_file_input.as_ref(),
        flow.enter_create_page(opts.step_timeout)
    );

    // 1) 上传
    werr(stderr, format_args!("步骤 1/5：上传视频…\n"));
    step!(
        Stage::Upload,
        selectors.video_file_input.as_ref(),
        flow.upload_video(&opts.video)
    );
    flow.require_logged_in().await?;

    // 2) 元信息
    werr(stderr, format_args!("步骤 2/5：填写标题/描述/标签…\n"));
    step!(
        Stage::Metadata,
        selectors.title_input.as_ref(),
        flow.fill_title(&opts.title)
    );
    step!(
        Stage::Metadata,
        selectors.description_editor.as_ref(),
        flow.fill_description(&opts.description)
    );
    step!(
        Stage::Metadata,
        selectors.topic_prefix.as_ref(),
        flow.fill_tags(&opts.tags)
    );

    // 3) 封面
    if let Some(cover) = &opts.cover {
        werr(stderr, format_args!("步骤 3/5：设置封面…\n"));
        step!(
            Stage::Cover,
            selectors.cover_file_input.as_ref(),
            flow.set_cover(cover)
        );
    }

    // 4) 声明（默认保守不勾原创）
    werr(stderr, format_args!("步骤 4/5：核对声明选项…\n"));
    step!(
        Stage::Declaration,
        selectors.original_declaration_label.as_ref(),
        flow.check_declaration_conservative()
    );

    if opts.dry_run {
        werr(
            stderr,
            format_args!("步骤 5/5：dry-run 到此为止，未提交。可去掉 --dry-run 正式发布。\n"),
        );
        return Ok(PublishResult {
            account: account_name.to_string(),
            video: opts.video.to_string_lossy().into_owned(),
            title: opts.title.clone(),
            dry_run: true,
            submitted: false,
            scheduled_at: opts.schedule_at.map(|t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            }),
        });
    }

    // 5) 提交
    werr(stderr, format_args!("步骤 5/5：提交发布…\n"));
    step!(
        Stage::Submit,
        selectors.submit_button.as_ref(),
        flow.submit()
    );
    werr(stderr, format_args!("发布已提交。\n"));

    Ok(PublishResult {
        account: account_name.to_string(),
        video: opts.video.to_string_lossy().into_owned(),
        title: opts.title.clone(),
        dry_run: false,
        submitted: true,
        scheduled_at: opts.schedule_at.map(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default()
        }),
    })
}

async fn navigate(
    page: &chromiumoxide::Page,
    url: &str,
    cancel: &crate::http::CancelToken,
) -> Result<()> {
    let cancel_fut = async {
        while !cancel.cancelled() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::pin!(cancel_fut);
    tokio::select! {
        _ = &mut cancel_fut => Err(AppError::new(Code::Cancelled, Stage::Navigate, "导航已取消")),
        r = page.goto(url) => {
            r.map_err(|e| AppError::fmt(Code::NetworkError, Stage::Navigate, format_args!("无法打开发布页面: {e}")))?;
            let _ = tokio::time::timeout(NAVIGATE_TIMEOUT, page.wait_for_navigation()).await;
            Ok(())
        }
    }
}

/// 供 CLI 构造默认 Options 的便利函数。
#[allow(clippy::too_many_arguments)]
pub fn default_options(
    video: PathBuf,
    title: String,
    description: String,
    tags: Vec<String>,
    cover: Option<PathBuf>,
    dry_run: bool,
    headed: bool,
    timeout: Duration,
    cancel: crate::http::CancelToken,
    stderr: SharedWriter,
) -> Options {
    Options {
        video,
        title,
        description,
        tags,
        cover,
        dry_run,
        headed,
        timeout,
        cancel,
        stderr,
        open_page: None,
        selectors: None,
        navigate_url: None,
        step_timeout: Duration::from_secs(60),
        schedule_at: None,
    }
}
