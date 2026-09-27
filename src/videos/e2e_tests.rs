//! 视频管理测试：真实 Chromium 加载 fixture 页面的离线 e2e。
//!
//! 模式同 publish/e2e_tests.rs：SPH_CHROME 或 rod 缓存提供 Chromium；
//! 列表数据由 fixture 里的 window.fetch stub 提供（file:// 无需真后端）。

#[cfg(test)]
pub(crate) mod tests {
    use crate::apperr::Code;
    use crate::browser;
    use crate::http::CancelToken;
    use crate::publish::mod_impl::OpenedPage;
    use crate::session::{accounts_root, AccountDir, DEFAULT_ACCOUNT};
    use crate::videos::edit::{run_edit, validate_edit, EditOptions};
    use crate::videos::list::{run_list, ListOptions};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const FIXTURE_LIST: &str = "videos_list_fixture.html";
    const FIXTURE_EDIT: &str = "videos_edit_fixture.html";
    const FIXTURE_LOGIN: &str = "publish_login_fixture.html";

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-videos-test-{tag}-{:016x}", {
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

    fn list_opts(fixture: &str) -> (ListOptions, Arc<Mutex<Vec<u8>>>) {
        let chrome = test_chrome().expect("需要 Chromium（SPH_CHROME 或 rod 缓存）");
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let opts = ListOptions {
            collection: None,
            limit: 20,
            headed: false,
            timeout: Duration::from_secs(60),
            cancel: CancelToken::default(),
            stderr: stderr_vec.clone(),
            open_page: Some(Arc::new(move |_profile, _headed| {
                let chrome = chrome.clone();
                Box::pin(async move {
                    let tmp = tempdir("openpage");
                    let (b, page) = browser::launch(&chrome, &tmp, false).await?;
                    Ok(OpenedPage {
                        browser: Some(b),
                        page,
                    })
                })
            })),
            navigate_url: Some(fixture_url(fixture)),
            selectors: None,
        };
        (opts, stderr_vec)
    }

    fn edit_opts(fixture: &str, id: &str, dry_run: bool) -> (EditOptions, Arc<Mutex<Vec<u8>>>) {
        let chrome = test_chrome().expect("需要 Chromium（SPH_CHROME 或 rod 缓存）");
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let opts = EditOptions {
            id: id.to_string(),
            // 平台短标题限制 6..=16 字
            title: Some("新标题三个字".to_string()),
            description: Some("新描述".to_string()),
            cover: None,
            cover_landscape: None,
            dry_run,
            headed: false,
            timeout: Duration::from_secs(60),
            cancel: CancelToken::default(),
            stderr: stderr_vec.clone(),
            open_page: Some(Arc::new(move |_profile, _headed| {
                let chrome = chrome.clone();
                Box::pin(async move {
                    let tmp = tempdir("openpage");
                    let (b, page) = browser::launch(&chrome, &tmp, false).await?;
                    Ok(OpenedPage {
                        browser: Some(b),
                        page,
                    })
                })
            })),
            navigate_url: Some(fixture_url(fixture)),
            selectors: None,
        };
        (opts, stderr_vec)
    }

    #[tokio::test]
    async fn e2e_list_paginates_and_filters() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("list");
        make_session(&config);

        // 全量：5 条（跨 3 页）；post_list 本身不含合集归属（真实接口亦如此）
        let (opts, _) = list_opts(FIXTURE_LIST);
        let result = run_list(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert_eq!(result.count, 5);
        assert_eq!(result.videos[0].id, "v1");
        assert_eq!(result.videos[0].title, "齿轮是怎么工作的");
        assert_eq!(result.videos[0].collection, None);
        assert!(!result.client_side_collection_filter);

        // 合集过滤：服务端成员接口返回机械系列 2 条
        let (mut opts, _) = list_opts(FIXTURE_LIST);
        opts.collection = Some("机械系列".to_string());
        let result = run_list(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert_eq!(result.count, 2);
        assert!(!result.client_side_collection_filter);
        assert!(result
            .videos
            .iter()
            .all(|v| v.collection.as_deref() == Some("机械系列")));

        // 不存在的合集 → 参数错误（列出可用合集名）
        let (mut opts, _) = list_opts(FIXTURE_LIST);
        opts.collection = Some("不存在".to_string());
        let err = run_list(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::InvalidArgument);
        assert!(err.message.contains("机械系列"));

        // limit 截断
        let (mut opts, _) = list_opts(FIXTURE_LIST);
        opts.limit = 1;
        let result = run_list(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert_eq!(result.count, 1);
    }

    #[tokio::test]
    async fn e2e_list_session_expired() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("listlogin");
        make_session(&config);
        let (opts, _) = list_opts(FIXTURE_LOGIN);
        let err = run_list(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::SessionExpired);
    }

    #[tokio::test]
    async fn e2e_edit_dry_run_fills_but_not_submits() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("editdry");
        make_session(&config);
        let (opts, _) = edit_opts(FIXTURE_EDIT, "v1", true);
        validate_edit(&opts).unwrap();
        let result = run_edit(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.dry_run);
        assert!(!result.submitted);
        assert!(!result.verified);
        assert_eq!(result.changed, vec!["title", "description"]);
    }

    #[tokio::test]
    async fn e2e_edit_dry_run_with_cover() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("editcover");
        make_session(&config);
        // 假封面文件（set_file_input 只要求文件存在）
        let cover = config.join("cover.png");
        std::fs::write(&cover, b"\x89PNG\r\n\x1a\n fixture").unwrap();
        let (mut opts, _) = edit_opts(FIXTURE_EDIT, "v1", true);
        opts.cover = Some(cover);
        let result = run_edit(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.dry_run);
        assert!(!result.submitted);
        assert_eq!(result.changed, vec!["title", "description", "cover"]);
    }

    #[tokio::test]
    async fn e2e_edit_submit_and_verify() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("editsubmit");
        make_session(&config);
        let (opts, _) = edit_opts(FIXTURE_EDIT, "v1", false);
        let result = run_edit(&config, DEFAULT_ACCOUNT, opts).await.unwrap();
        assert!(result.submitted);
        assert!(result.verified, "提交后复检应通过（fixture 写回 DATA）");
    }

    #[tokio::test]
    async fn e2e_edit_platform_reject() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("editreject");
        make_session(&config);
        let (mut opts, _) = edit_opts(FIXTURE_EDIT, "v1", false);
        opts.navigate_url = Some(format!("{}?editReject=1", fixture_url(FIXTURE_EDIT)));
        let err = run_edit(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::PublishRejected);
        assert!(err.message.contains("平台拒绝"));
    }

    #[tokio::test]
    async fn e2e_edit_unknown_id_is_video_unavailable() {
        if test_chrome().is_none() {
            eprintln!("skip: no chromium");
            return;
        }
        let config = tempdir("edit404");
        make_session(&config);
        let (opts, _) = edit_opts(FIXTURE_EDIT, "nope", false);
        let err = run_edit(&config, DEFAULT_ACCOUNT, opts).await.unwrap_err();
        assert_eq!(err.code, Code::VideoUnavailable);
    }
}
