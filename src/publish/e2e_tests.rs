//! 发布流水线测试：真实 Chromium 加载 fixture 页面的离线 e2e。
//!
//! 测试需要本机有 Chromium：SPH_CHROME 环境变量或 rod 缓存；CI 经 SPH_CHROME 注入。

#[cfg(test)]
pub(crate) mod tests {
    use crate::apperr::{Code, Stage};
    use crate::browser::{self, SharedWriter};
    use crate::cli::run::parse_schedule_at;
    use crate::http::CancelToken;
    use crate::publish::mod_impl::{
        default_options, run, validate, OpenPageFn, OpenedPage, Options,
    };
    use crate::publish::page::DEFAULT_SELECTORS;
    use crate::session::{accounts_root, AccountDir, DEFAULT_ACCOUNT};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const FIXTURE_PUBLISH: &str = "publish_fixture.html";
    const FIXTURE_LOGIN: &str = "publish_login_fixture.html";
    const FIXTURE_REJECT: &str = "publish_reject_fixture.html";
    const FIXTURE_DECLARED: &str = "publish_declared_fixture.html";

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-pub-test-{tag}-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn fixture_url(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        format!("file://{}", path.display())
    }

    /// 找本机 Chromium；找不到返回 None（测试跳过）。
    fn test_chrome() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("SPH_CHROME") {
            if !p.is_empty() && Path::new(&p).exists() {
                return Some(PathBuf::from(p));
            }
        }
        #[cfg(target_os = "macos")]
        {
            let rod = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".cache/rod/browser");
            if let Ok(rd) = std::fs::read_dir(&rod) {
                let mut candidates: Vec<PathBuf> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .map(|n| n.to_string_lossy().starts_with("chromium-"))
                            .unwrap_or(false)
                    })
                    .collect();
                candidates.sort();
                for c in candidates.iter().rev() {
                    let bin = c.join("Chromium.app/Contents/MacOS/Chromium");
                    if bin.exists() {
                        return Some(bin);
                    }
                }
            }
        }
        None
    }

    fn make_session(config_dir: &Path) {
        let dir = AccountDir::new(&accounts_root(config_dir), DEFAULT_ACCOUNT);
        std::fs::create_dir_all(dir.profile_dir()).unwrap();
    }

    fn test_video_file(dir: &Path) -> PathBuf {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/tiny.mp4");
        let dst = dir.join("video.mp4");
        std::fs::copy(&src, &dst).unwrap();
        dst
    }

    fn test_cover_file(dir: &Path) -> PathBuf {
        let dst = dir.join("cover.jpg");
        std::fs::write(&dst, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]).unwrap();
        dst
    }

    /// 真实 Chromium 页面工厂（fixture 测试共用）。
    fn fixture_open_page() -> Option<OpenPageFn> {
        let chrome = test_chrome()?;
        Some(Arc::new(move |_profile, _headed| {
            let chrome = chrome.clone();
            Box::pin(async move {
                let tmp = tempdir("openpage");
                let (b, page) = browser::launch(&chrome, &tmp, false).await?;
                Ok(OpenedPage {
                    browser: Some(b),
                    page,
                })
            })
        }))
    }

    /// 构造 opts：注入 open_page（真实 Chromium 加载 fixture）与导航覆盖。
    /// 返回 (Options, stderr 捕获) 便于断言。
    fn opts_with_fixture(
        fixture: &str,
        video: PathBuf,
        cover: Option<PathBuf>,
        dry_run: bool,
    ) -> (Options, Arc<Mutex<Vec<u8>>>) {
        let chrome = test_chrome().expect("需要 Chromium（SPH_CHROME 或 rod 缓存）");
        let url = fixture_url(fixture);
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_writer: SharedWriter = stderr_vec.clone();
        let mut opts = default_options(
            video,
            "齿轮是怎么工作的".to_string(),
            "一个描述".to_string(),
            vec!["机械".to_string(), "科普".to_string()],
            cover,
            dry_run,
            false,
            Duration::from_secs(60),
            CancelToken::default(),
            stderr_writer,
        );
        opts.navigate_url = Some(url.clone());
        opts.open_page = Some(Arc::new(move |_profile, _headed| {
            let chrome = chrome.clone();
            Box::pin(async move {
                let tmp = tempdir("openpage");
                let (b, page) = browser::launch(&chrome, &tmp, false).await?;
                Ok(OpenedPage {
                    browser: Some(b),
                    page,
                })
            })
        }));
        (opts, stderr_vec)
    }

    #[tokio::test]
    async fn e2e_publish_happy_path() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("happy");
        make_session(&config);
        let work = tempdir("work");
        let video = test_video_file(&work);
        let cover = test_cover_file(&work);
        let (opts, _stderr_vec) = opts_with_fixture(FIXTURE_PUBLISH, video, Some(cover), false);
        let result = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.submitted);
        assert!(!result.dry_run);
        assert_eq!(result.account, "default");
        assert_eq!(result.title, "齿轮是怎么工作的");
    }

    #[tokio::test]
    async fn e2e_publish_dry_run_stops_before_submit() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("dry");
        make_session(&config);
        let work = tempdir("workdry");
        let video = test_video_file(&work);
        let (opts, stderr_vec) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        let result = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(!result.submitted);
        assert!(result.dry_run);
        let out = String::from_utf8_lossy(&stderr_vec.lock().unwrap()).into_owned();
        assert!(out.contains("dry-run"), "stderr: {out}");
    }

    #[tokio::test]
    async fn e2e_session_missing_is_expired() {
        let config = tempdir("nosession");
        let work = tempdir("workns");
        let video = test_video_file(&work);
        let (opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, false);
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::SessionExpired);
        assert_eq!(err.stage, Stage::SessionLoad);
    }

    #[tokio::test]
    async fn e2e_login_page_is_expired() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("loginpage");
        make_session(&config);
        let work = tempdir("worklp");
        let video = test_video_file(&work);
        let (opts, _stderr) = opts_with_fixture(FIXTURE_LOGIN, video, None, false);
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::SessionExpired);
        assert_eq!(err.stage, Stage::Navigate);
        assert!(err.message.contains("sph login"));
    }

    #[tokio::test]
    async fn e2e_platform_reject_is_publish_rejected() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("reject");
        make_session(&config);
        let work = tempdir("workrj");
        let video = test_video_file(&work);
        let (opts, _stderr) = opts_with_fixture(FIXTURE_REJECT, video, None, false);
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::PublishRejected, "err: {err}");
        assert_eq!(err.stage, Stage::Submit);
        assert!(err.message.contains("内容审核未通过"));
        assert!(!err.message.contains("<b>"), "平台提示必须经消毒");
    }

    #[tokio::test]
    async fn e2e_default_checked_declaration_is_rejected() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("declared");
        make_session(&config);
        let work = tempdir("workdc");
        let video = test_video_file(&work);
        let (opts, _stderr) = opts_with_fixture(FIXTURE_DECLARED, video, None, false);
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::PublishRejected, "err: {err}");
        assert_eq!(err.stage, Stage::Declaration);
    }

    #[test]
    fn validate_rejects_bad_inputs() {
        let work = tempdir("workv");
        let video = test_video_file(&work);
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_writer: SharedWriter = stderr_vec.clone();

        // 空标题
        let mut opts = default_options(
            video.clone(),
            String::new(),
            String::new(),
            vec![],
            None,
            false,
            false,
            Duration::from_secs(60),
            CancelToken::default(),
            stderr_writer,
        );
        assert_eq!(validate(&opts).unwrap_err().code, Code::InvalidArgument);
        // 视频不存在
        opts.video = work.join("nope.mp4");
        opts.title = "t".into();
        assert_eq!(validate(&opts).unwrap_err().code, Code::InvalidArgument);
        // 空视频文件
        let empty = work.join("empty.mp4");
        std::fs::write(&empty, b"").unwrap();
        opts.video = empty;
        assert_eq!(validate(&opts).unwrap_err().code, Code::InvalidArgument);
        // 封面不存在
        opts.video = video.clone();
        opts.cover = Some(work.join("nope.jpg"));
        assert_eq!(validate(&opts).unwrap_err().code, Code::InvalidArgument);
        // 合法组合
        opts.cover = None;
        assert!(validate(&opts).is_ok());
    }

    #[tokio::test]
    async fn e2e_batch_dry_run_multiple_videos() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("batch");
        make_session(&config);
        // 构造目录：两个 mp4 + 一个同名封面
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/tiny.mp4");
        let dir = tempdir("videos");
        for name in ["齿轮原理", "杠杆实验"] {
            std::fs::copy(&src, dir.join(format!("{name}.mp4"))).unwrap();
        }
        std::fs::write(dir.join("齿轮原理.jpg"), b"\xff\xd8fake-jpeg").unwrap();

        // 逐条走 publish::run（与 batch 命令同一入口逻辑）
        let mut ok_count = 0;
        for name in ["齿轮原理", "杠杆实验"] {
            let video = dir.join(format!("{name}.mp4"));
            let cover = ["jpg", "jpeg", "png"]
                .iter()
                .map(|ext| video.with_extension(ext))
                .find(|p| p.is_file());
            let opts = default_options(
                video,
                name.to_string(),
                String::new(),
                vec![],
                cover,
                true, // dry-run
                false,
                Duration::from_secs(60),
                CancelToken::default(),
                Arc::new(Mutex::new(Vec::new())) as SharedWriter,
            );
            // 手工构造必须覆盖导航 URL（否则打到真实平台）
            let mut opts = opts;
            opts.navigate_url = Some(fixture_url(FIXTURE_PUBLISH));
            opts.open_page = fixture_open_page();
            validate(&opts).unwrap();
            let r = match run(&config, DEFAULT_ACCOUNT, opts).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("batch run failed for {name}: {e}");
                    panic!("run failed: {e}");
                }
            };
            assert!(r.dry_run && !r.submitted);
            assert_eq!(r.title, name.to_string());
            ok_count += 1;
        }
        assert_eq!(ok_count, 2);
    }

    #[test]
    fn selectors_default_is_complete() {
        let s = &DEFAULT_SELECTORS;
        let all = [
            s.home_ready.as_ref(),
            s.login_indicator.as_ref(),
            s.publish_entry.as_ref(),
            s.video_file_input.as_ref(),
            s.upload_done_indicator.as_ref(),
            s.title_input.as_ref(),
            s.description_editor.as_ref(),
            s.topic_prefix.as_ref(),
            s.cover_file_input.as_ref(),
            s.original_declaration_checkbox.as_ref(),
            s.submit_button.as_ref(),
            s.publish_success_indicator.as_ref(),
            s.publish_error_indicator.as_ref(),
        ];
        for sel in all {
            assert!(!sel.is_empty());
        }
        assert_eq!(all.len(), 13);
    }
    use crate::publish::recovery::RecoveryAction;

    // 失败现场保存 e2e：补丁把 home_ready 改成不存在的 selector，
    // 步骤超时 → 快照 + 截图落盘 config/crashes/，轨迹进 history.jsonl。
    #[tokio::test]
    async fn m3_failure_saves_scene_and_trace() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("m3scene");
        make_session(&config);
        // 补丁：home_ready 指向不存在的元素
        std::fs::create_dir_all(config.join("patches")).unwrap();
        std::fs::write(
            config.join("patches").join("publish.json"),
            br#"{"selectors": {"home_ready": ".never-exists"}}"#,
        )
        .unwrap();

        let work = tempdir("workm3");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        opts.step_timeout = Duration::from_secs(2);

        // selector 直接注入（补丁文件加载路径已由单测覆盖）：home_ready 指向不存在的元素
        let mut patched = crate::publish::page::DEFAULT_SELECTORS.clone();
        patched.home_ready = std::borrow::Cow::Borrowed(".never-exists");
        opts.selectors = Some(patched);
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        // Timeout 走恢复路径（RuleBackend 等待后重试），仍失败 → 17
        assert_eq!(err.code, Code::RecoveryFailed, "err: {err}");

        // 现场保存：crashes/<dir>/{snapshot.json,page.txt,screenshot.png}
        let crashes = std::fs::read_dir(config.join("crashes")).unwrap();
        let scene = crashes
            .filter_map(|e| e.ok())
            .next()
            .expect("scene dir must exist");
        assert!(scene.path().join("snapshot.json").exists());
        assert!(scene.path().join("page.txt").exists());
        assert!(scene.path().join("screenshot.png").exists());
        let snapshot = std::fs::read_to_string(scene.path().join("snapshot.json")).unwrap();
        assert!(snapshot.contains("\"stage\": \"navigate\""));
        assert!(snapshot.contains("never-exists"));

        // 轨迹落盘 history.jsonl：RuleBackend 被执行且重试失败
        let history = std::fs::read_to_string(config.join("history.jsonl")).unwrap();
        assert!(
            history.contains("\"backend\":\"rule\""),
            "history: {history}"
        );
        assert!(history.contains("retry_failed"), "history: {history}");
    }

    #[test]
    fn recovery_action_wait_is_cloneable() {
        let a = RecoveryAction::Wait(Duration::from_millis(5));
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn parse_schedule_at_formats() {
        // 合法：未来时间
        let at = parse_schedule_at("2999-01-01 08:30").unwrap();
        assert_eq!(at.hour(), 8);
        assert_eq!(at.minute(), 30);
        // T 分隔也接受
        let at2 = parse_schedule_at("2999-01-01T08:30").unwrap();
        assert_eq!(at2.hour(), 8);
        // 过去时间拒绝
        let err = parse_schedule_at("2020-01-01 08:30").unwrap_err();
        assert_eq!(err.code, Code::ScheduleInvalid);
        // 坏格式拒绝
        assert_eq!(
            parse_schedule_at("nope").unwrap_err().code,
            Code::ScheduleInvalid
        );
        assert_eq!(
            parse_schedule_at("2026-13-01 08:30").unwrap_err().code,
            Code::ScheduleInvalid
        );
        assert_eq!(
            parse_schedule_at("2999-01-01 25:00").unwrap_err().code,
            Code::ScheduleInvalid
        );
    }

    #[tokio::test]
    async fn e2e_publish_with_schedule_dry_run() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("sched");
        make_session(&config);
        let work = tempdir("worksched");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        // 本地时区未来 2 小时
        let now_local = time::OffsetDateTime::now_local().unwrap();
        opts.schedule_at = Some(now_local + Duration::from_secs(2 * 3600));
        let result = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.dry_run);
        assert!(
            result.scheduled_at.is_some(),
            "scheduled_at must be reported"
        );
    }

    #[tokio::test]
    async fn e2e_publish_with_extended_attributes() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("attrs");
        make_session(&config);
        let work = tempdir("workattrs");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        opts.collection = Some("机械系列".to_string());
        opts.link = Some("公众号文章".to_string());
        opts.activity = Some("科学实验挑战赛".to_string());
        opts.ai_mark = true;
        let result = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.dry_run);
        // 校验 fixture 回填：合集/活动 placeholder 已变成选项文案；标注已勾选
        // （通过 history 无失败轨迹间接确认全部步骤通过）
        let history = std::fs::read_to_string(config.join("history.jsonl")).unwrap_or_default();
        assert!(!history.contains("retry_failed"), "history: {history}");
    }

    #[tokio::test]
    async fn e2e_ai_mark_only() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("aimark");
        make_session(&config);
        let work = tempdir("workaimark");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        opts.ai_mark = true;
        let result = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.dry_run);
    }

    #[tokio::test]
    async fn e2e_collection_not_found_is_loud_error() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("attrfail");
        make_session(&config);
        let work = tempdir("workattrfail");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        opts.collection = Some("不存在的合集".to_string());
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::SchemaChanged, "err: {err}");
        assert!(err.message.contains("不存在的合集"), "err: {err}");
    }

    #[tokio::test]
    async fn e2e_collection_click_without_selection_stops_before_submit() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("collection-noop");
        make_session(&config);
        let work = tempdir("collection-noop-video");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, false);
        opts.navigate_url = Some(format!("{}?collectionNoop=1", fixture_url(FIXTURE_PUBLISH)));
        opts.collection = Some("机械系列".to_string());
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::SchemaChanged, "err: {err}");
        assert!(err.message.contains("未回填"), "err: {err}");
    }

    #[tokio::test]
    async fn e2e_collection_reset_by_schedule_stops_before_submit() {
        let Some(_chrome) = test_chrome() else {
            eprintln!("skip: no chromium");
            return;
        };
        let config = tempdir("collection-reset");
        make_session(&config);
        let work = tempdir("collection-reset-video");
        let video = test_video_file(&work);
        let (mut opts, _stderr) = opts_with_fixture(FIXTURE_PUBLISH, video, None, true);
        opts.navigate_url = Some(format!(
            "{}?collectionResetOnSchedule=1",
            fixture_url(FIXTURE_PUBLISH)
        ));
        opts.collection = Some("机械系列".to_string());
        opts.schedule_at =
            Some(time::OffsetDateTime::now_local().unwrap() + Duration::from_secs(7200));
        let err = run(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::SchemaChanged, "err: {err}");
        assert!(err.message.contains("提交前"), "err: {err}");
    }
}
