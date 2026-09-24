//! 发布流水线测试：真实 Chromium 加载 fixture 页面的离线 e2e。
//!
//! 测试需要本机有 Chromium：SPH_CHROME 环境变量或 rod 缓存；CI 经 SPH_CHROME 注入。

#[cfg(test)]
mod tests {
    use crate::apperr::{Code, Stage};
    use crate::browser::{self, SharedWriter};
    use crate::http::CancelToken;
    use crate::publish::mod_impl::{default_options, run, validate, OpenedPage, Options};
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

    #[test]
    fn selectors_default_is_complete() {
        let s = &DEFAULT_SELECTORS;
        let all = [
            s.home_ready,
            s.login_indicator,
            s.publish_entry,
            s.video_file_input,
            s.upload_done_indicator,
            s.title_input,
            s.description_editor,
            s.topic_prefix,
            s.cover_file_input,
            s.original_declaration_checkbox,
            s.submit_button,
            s.publish_success_indicator,
            s.publish_error_indicator,
        ];
        for sel in all {
            assert!(!sel.is_empty());
        }
        assert_eq!(all.len(), 13);
    }
}
