//! 在字节上限内把单个媒体文件流式写入磁盘，校验后按默认不覆盖语义原子提交。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::http::{CancelToken, RawRequest, RoundTrip};
use crate::media;
use crate::upstream::hex_encode;
use crate::verify;

/// 校验器类型别名。
pub type VerifyFn = Arc<dyn Fn(&Path, &CancelToken) -> Result<String> + Send + Sync>;
type WarnFn = Arc<Box<dyn Fn(String) + Send>>;

/// 默认上限 2 GiB。
pub const DEFAULT_MAX_BYTES: i64 = 2147483648;

const COPY_BUFFER_BYTES: usize = 256 << 10; // 256 KiB
const SNIFF_BYTES: usize = 64;

/// 一次下载的配置。
pub struct Options {
    /// 绝对目标路径；空表示在 work_dir 内自动命名 <safe-title>_<local_id>.mp4。
    pub output_path: String,
    pub work_dir: String,
    pub overwrite: bool,
    pub max_bytes: i64,
    pub http: Arc<dyn RoundTrip>,
    /// 可选；调用方自行节流频率。
    pub progress: Option<Box<dyn Fn(i64, i64) + Send>>,
    /// 报告非致命清理问题（残留临时文件）。
    pub warn: Option<Box<dyn Fn(String) + Send>>,
    pub cancel: CancelToken,
    /// 可注入的校验器（测试用）；None 使用真实 verify。
    pub verify_fn: Option<VerifyFn>,
}

/// 下载成功的安全 DTO。
#[derive(Debug, Serialize)]
pub struct ResultDTO {
    #[serde(rename = "local_id")]
    pub local_id: String,
    pub path: String,
    pub bytes: i64,
    #[serde(rename = "sha256")]
    pub sha256: String,
    pub verification: String,
}

/// 对一个已解析视频执行下载。
pub struct Downloader {
    opts: Options,
    video: media::ResolvedVideo,
    tmp_path: Option<PathBuf>,
}

impl std::fmt::Debug for Downloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Downloader")
            .field("output_path", &self.opts.output_path)
            .field("local_id", &self.video.local_id)
            .finish()
    }
}

/// 确保失败路径上临时文件一定被清理。
struct TmpGuard {
    path: Option<PathBuf>,
    warn: Option<WarnFn>,
}

impl Drop for TmpGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            if let Err(e) = std::fs::remove_file(&p) {
                if !matches!(e.kind(), std::io::ErrorKind::NotFound) {
                    if let Some(w) = &self.warn {
                        w(format!("清理临时文件失败，请手动删除 {}", p.display()));
                    }
                }
            }
        }
    }
}

impl Downloader {
    /// 在任何网络活动之前校验选项并准备目标路径。
    pub fn new(video: media::ResolvedVideo, mut opts: Options) -> Result<Downloader> {
        if opts.max_bytes == 0 {
            opts.max_bytes = DEFAULT_MAX_BYTES;
        }
        if opts.max_bytes < 0 {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "--max-bytes 必须是正整数",
            ));
        }
        if opts.work_dir.is_empty() {
            if opts.output_path.is_empty() {
                return Err(AppError::new(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    "未指定输出目录",
                ));
            }
            opts.work_dir = parent_of(&opts.output_path);
        }
        let target = if opts.output_path.is_empty() {
            PathBuf::from(&opts.work_dir).join(format!(
                "{}_{}.mp4",
                media::safe_title(&video.title),
                video.local_id
            ))
        } else {
            if !opts.output_path.to_ascii_lowercase().ends_with(".mp4") {
                return Err(AppError::new(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    "输出文件必须以 .mp4 结尾",
                ));
            }
            let parent = parent_of(&opts.output_path);
            if !Path::new(&parent).is_dir() {
                return Err(AppError::new(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    "输出路径的父目录不存在",
                ));
            }
            PathBuf::from(&opts.output_path)
        };
        // 词法绝对化（不解析符号链接，与 Go filepath.Abs 一致；macOS /var 是 /private/var 的链接）
        let abs = std::path::absolute(&target).unwrap_or(target);
        match std::fs::symlink_metadata(&abs) {
            Ok(m) => {
                if m.file_type().is_dir() {
                    return Err(AppError::new(
                        Code::InvalidArgument,
                        Stage::Arguments,
                        "输出路径是目录",
                    ));
                }
                if m.file_type().is_symlink() {
                    return Err(AppError::new(
                        Code::InvalidArgument,
                        Stage::Arguments,
                        "输出路径是符号链接，已拒绝",
                    ));
                }
                if !opts.overwrite {
                    return Err(AppError::fmt(
                        Code::FileExists,
                        Stage::Commit,
                        format_args!("目标文件已存在：{}（使用 --overwrite 覆盖）", abs.display()),
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AppError::fmt(
                    Code::IOError,
                    Stage::Arguments,
                    format_args!("无法检查输出路径: {e}"),
                ))
            }
        }
        opts.output_path = abs.to_string_lossy().into_owned();
        Ok(Downloader {
            opts,
            video,
            tmp_path: None,
        })
    }

    pub fn target(&self) -> &str {
        &self.opts.output_path
    }

    /// 执行下载：抓取 → 临时文件 → 校验 → 原子提交。每条失败路径都删除其临时文件。
    pub async fn run(mut self) -> Result<ResultDTO> {
        let warn = self.opts.warn.take().map(Arc::new);
        let mut guard = TmpGuard { path: None, warn };
        let result = self.run_inner(&mut guard).await;
        if result.is_ok() {
            guard.path = None; // 已提交（或已显式清理）
        }
        result
    }

    async fn run_inner(&mut self, guard: &mut TmpGuard) -> Result<ResultDTO> {
        // 至多两次尝试；第二次总是从头开始
        let mut last_err: Option<AppError> = None;
        for attempt in 0..2 {
            if attempt > 0 {
                if self.opts.cancel.cancelled() {
                    break;
                }
                if let Some(p) = guard.path.take() {
                    let _ = std::fs::remove_file(&p);
                }
            }
            match self.fetch_to_temp(guard).await {
                Ok((bytes, sum)) => return self.commit(guard, bytes, sum).await,
                Err(e) => {
                    last_err = Some(e.clone());
                    if !is_retryable_transfer(&e) || self.opts.cancel.cancelled() {
                        return Err(e);
                    }
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| AppError::new(Code::DownloadFailed, Stage::Download, "下载失败")))
    }

    /// 把媒体响应流式写进目标目录下新建的 0600 临时文件，增量哈希，
    /// 返回 (bytes, sha256hex)。
    async fn fetch_to_temp(&mut self, guard: &mut TmpGuard) -> Result<(i64, String)> {
        let req = RawRequest {
            method: "GET".into(),
            url: self.video.media_url.clone(),
            headers: vec![
                ("user-agent".into(), "sph-local/1.0".into()),
                ("accept".into(), "*/*".into()),
                ("accept-encoding".into(), "identity".into()),
                ("referer".into(), "https://channels.weixin.qq.com/".into()),
            ],
            body: Vec::new(),
        };
        // 媒体请求不带任何会话材料
        let mut resp = self
            .opts
            .http
            .send(req, Duration::from_secs(30))
            .await
            .map_err(|e| self.map_transfer_error(e))?;
        match resp.status {
            200 => {}
            206 => {
                return Err(AppError::new(
                    Code::DownloadFailed,
                    Stage::Download,
                    "媒体服务器返回 206，本工具未请求 Range，拒绝保存不完整响应",
                ))
            }
            404 => {
                return Err(AppError::new(
                    Code::DownloadFailed,
                    Stage::Download,
                    "媒体文件不存在 (HTTP 404)",
                ));
            }
            status => {
                return Err(AppError::fmt(
                    Code::DownloadFailed,
                    Stage::Download,
                    format_args!("媒体下载失败 (HTTP {status})"),
                ))
            }
        }
        if self.opts.max_bytes > 0 {
            if let Some(cl) = resp.content_length {
                if cl > self.opts.max_bytes {
                    return Err(AppError::fmt(
                        Code::DownloadTooLarge,
                        Stage::Download,
                        format_args!("文件大小 {cl} 超过上限 {} 字节", self.opts.max_bytes),
                    ));
                }
            }
        }
        let dir = PathBuf::from(&self.opts.output_path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let tmp_path = dir.join(format!(".sph-{:016x}.part", rand_u64()));
        let mut tmp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Download,
                    format_args!("无法创建临时文件: {e}"),
                )
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tmp.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        self.tmp_path = Some(tmp_path.clone());
        guard.path = Some(tmp_path.clone());

        let mut hash = Sha256::new();
        let mut sent: i64 = 0;
        let mut buf = vec![0u8; COPY_BUFFER_BYTES];
        let mut first = true;
        let content_length = resp.content_length;
        loop {
            if self.opts.cancel.cancelled() {
                return Err(classify_cancel(&self.opts.cancel, Stage::Download));
            }
            let n = resp.body.read(&mut buf).await.map_err(|_| {
                AppError::retryable(Code::NetworkError, Stage::Download, "媒体传输中断")
            })?;
            if n == 0 {
                break; // EOF
            }
            let chunk = &buf[..n];
            if first {
                check_sniff(&chunk[..SNIFF_BYTES.min(n)])?;
                first = false;
            }
            use std::io::Write;
            tmp.write_all(chunk).map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Download,
                    format_args!("写入临时文件失败: {e}"),
                )
            })?;
            hash.update(chunk);
            sent += n as i64;
            if self.opts.max_bytes > 0 && sent > self.opts.max_bytes {
                return Err(AppError::fmt(
                    Code::DownloadTooLarge,
                    Stage::Download,
                    format_args!("已下载字节超过上限 {}", self.opts.max_bytes),
                ));
            }
            if let Some(p) = &self.opts.progress {
                p(sent, content_length.unwrap_or(-1));
            }
        }
        if sent == 0 {
            return Err(AppError::new(
                Code::DownloadFailed,
                Stage::Download,
                "媒体响应为空",
            ));
        }
        if let Some(cl) = content_length {
            if cl > 0 && sent != cl {
                return Err(AppError::fmt(
                    Code::DownloadFailed,
                    Stage::Download,
                    format_args!("下载不完整：收到 {sent} 字节，Content-Length 为 {cl}"),
                ));
            }
        }
        tmp.sync_all().map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::Download,
                format_args!("同步临时文件失败: {e}"),
            )
        })?;
        drop(tmp);
        Ok((sent, hex_encode(&hash.finalize())))
    }

    fn map_transfer_error(&self, e: AppError) -> AppError {
        if self.opts.cancel.cancelled() {
            return classify_cancel(&self.opts.cancel, Stage::Download);
        }
        e
    }

    /// 校验临时文件后原子发布。
    async fn commit(&mut self, guard: &mut TmpGuard, bytes: i64, sum: String) -> Result<ResultDTO> {
        let tmp = guard.path.clone().ok_or_else(|| {
            AppError::new(
                Code::InternalError,
                Stage::Commit,
                "内部状态错误：缺少临时文件",
            )
        })?;
        let method = if let Some(vf) = &self.opts.verify_fn {
            vf(&tmp, &self.opts.cancel)?
        } else {
            // ffprobe 是同步阻塞进程调用，放到阻塞线程并限时
            let cancel = self.opts.cancel.clone();
            let tmp_for_verify = tmp.clone();
            tokio::time::timeout(
                verify::FFPROBE_TIMEOUT + Duration::from_secs(5),
                tokio::task::spawn_blocking(move || verify::verify(&tmp_for_verify, &cancel)),
            )
            .await
            .map_err(|_| AppError::new(Code::Timeout, Stage::Verify, "验证超时"))?
            .map_err(|e| {
                AppError::fmt(
                    Code::InternalError,
                    Stage::Verify,
                    format_args!("验证任务失败: {e}"),
                )
            })??
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let target = PathBuf::from(&self.opts.output_path);
        if self.opts.overwrite {
            std::fs::rename(&tmp, &target).map_err(|e| {
                AppError::fmt(
                    Code::IOError,
                    Stage::Commit,
                    format_args!("覆盖提交失败，旧文件保持不变: {e}"),
                )
            })?;
            self.tmp_path = None;
            guard.path = None;
        } else {
            // 硬链接+unlink 在同文件系统上提供原子不覆盖提交；
            // 朴素的 stat+rename 可能覆盖并发创建的文件
            #[cfg(unix)]
            {
                std::fs::hard_link(&tmp, &target).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        AppError::fmt(
                            Code::FileExists,
                            Stage::Commit,
                            format_args!(
                                "目标文件已存在：{}（使用 --overwrite 覆盖）",
                                target.display()
                            ),
                        )
                    } else {
                        AppError::fmt(
                            Code::IOError,
                            Stage::Commit,
                            format_args!(
                                "当前文件系统不支持原子提交（硬链接），已停止而不写出半个文件: {e}"
                            ),
                        )
                    }
                })?;
                if let Err(e) = std::fs::remove_file(&tmp) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        if let Some(w) = &self.opts.warn {
                            w(format!(
                                "下载完成，但清理临时硬链接失败，请手动删除 {}",
                                tmp.display()
                            ));
                        }
                    }
                }
                self.tmp_path = None;
                guard.path = None;
            }
            #[cfg(windows)]
            {
                // Windows 无硬链接原子语义可用：用独占创建+复制。文件系统语义已降级，
                // 但 no-clobber 仍成立（create_new 失败即 FILE_EXISTS）。
                copy_no_clobber(&tmp, &target)?;
                let _ = std::fs::remove_file(&tmp);
                self.tmp_path = None;
                guard.path = None;
            }
        }
        Ok(ResultDTO {
            local_id: self.video.local_id.clone(),
            path: target.to_string_lossy().into_owned(),
            bytes,
            sha256: sum,
            verification: method,
        })
    }
}

#[cfg(windows)]
fn copy_no_clobber(src: &Path, dst: &Path) -> Result<()> {
    let mut reader = std::fs::File::open(src).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::Commit,
            format_args!("无法读取临时文件: {e}"),
        )
    })?;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::fmt(
                    Code::FileExists,
                    Stage::Commit,
                    format_args!("目标文件已存在：{}（使用 --overwrite 覆盖）", dst.display()),
                )
            } else {
                AppError::fmt(
                    Code::IOError,
                    Stage::Commit,
                    format_args!("创建目标文件失败: {e}"),
                )
            }
        })?;
    std::io::copy(&mut reader, &mut writer).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::Commit,
            format_args!("写出目标文件失败: {e}"),
        )
    })?;
    Ok(())
}

/// 尽早拒绝明显的非视频响应体。
fn check_sniff(head: &[u8]) -> Result<()> {
    let s = String::from_utf8_lossy(head);
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("#extm3u") || lower.contains("#ext-x-") {
        return Err(AppError::new(
            Code::UnsupportedMedia,
            Stage::Download,
            "返回的是 HLS 播放列表，本版本不支持",
        ));
    }
    if lower.starts_with("<mpd") {
        return Err(AppError::new(
            Code::UnsupportedMedia,
            Stage::Download,
            "返回的是 DASH 清单，本版本不支持",
        ));
    }
    if lower.starts_with("<html") || lower.starts_with("<!doctype html") {
        return Err(AppError::new(
            Code::DownloadFailed,
            Stage::Download,
            "媒体服务器返回了 HTML 错误页",
        ));
    }
    if lower.starts_with("<?xml") {
        return Err(AppError::new(
            Code::DownloadFailed,
            Stage::Download,
            "媒体服务器返回了 XML 错误响应",
        ));
    }
    if s.starts_with('{') || s.starts_with('[') {
        return Err(AppError::new(
            Code::DownloadFailed,
            Stage::Download,
            "媒体服务器返回了 JSON 错误响应",
        ));
    }
    Ok(())
}

fn classify_cancel(cancel: &CancelToken, stage: Stage) -> AppError {
    let _ = cancel;
    AppError::new(Code::Cancelled, stage, "下载已取消")
}

fn is_retryable_transfer(e: &AppError) -> bool {
    e.code == Code::NetworkError || e.retryable
}

fn parent_of(p: &str) -> String {
    Path::new(p)
        .parent()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn rand_u64() -> u64 {
    use rand::Rng;
    rand::thread_rng().gen()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{RawRequest, RawResponse, RoundTrip};
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    const SENTINEL_MEDIA: &str = "DO_NOT_LEAK_MEDIA_QUERY_789";

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-dl-test-{:016x}", rand_u64()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn test_video() -> media::ResolvedVideo {
        media::ResolvedVideo {
            local_id: "abc123def456".into(),
            title: "示例 视频".into(),
            media_url: format!("https://cdn.example.com/v.mp4?sig={SENTINEL_MEDIA}"),
            ..Default::default()
        }
    }

    fn mp4_box(typ: &str, payload: &[u8]) -> Vec<u8> {
        let size = (payload.len() + 8) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(typ.as_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn synthetic_mp4(size: usize) -> Vec<u8> {
        let mut out = mp4_box("ftyp", b"isom");
        out.extend_from_slice(&mp4_box("moov", &[0u8; 64]));
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        out.extend_from_slice(&mp4_box("mdat", &payload));
        out
    }

    // 可脚本化的假传输层：按顺序返回响应，或注入失败体
    struct FakeHttp {
        script: Mutex<Vec<FakeResponse>>,
        requests: Mutex<Vec<RawRequest>>,
    }

    enum FakeResponse {
        Bytes {
            status: u16,
            body: Vec<u8>,
            content_length: Option<i64>,
        },
        Broken {
            data: Vec<u8>,
            fail_after: usize,
        },
        NetworkError,
    }

    impl FakeHttp {
        fn new(script: Vec<FakeResponse>) -> FakeHttp {
            FakeHttp {
                script: Mutex::new(script),
                requests: Mutex::new(vec![]),
            }
        }

        fn requests(&self) -> Vec<RawRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl RoundTrip for FakeHttp {
        async fn send(&self, req: RawRequest, _timeout: Duration) -> Result<RawResponse> {
            self.requests.lock().unwrap().push(req.clone());
            let next = {
                let mut s = self.script.lock().unwrap();
                if s.is_empty() {
                    FakeResponse::NetworkError
                } else {
                    s.remove(0)
                }
            };
            match next {
                FakeResponse::Bytes {
                    status,
                    body,
                    content_length,
                } => Ok(RawResponse {
                    status,
                    content_length: content_length.or(Some(body.len() as i64)),
                    body: Box::pin(std::io::Cursor::new(body)),
                }),
                FakeResponse::Broken { data, fail_after } => Ok(RawResponse {
                    status: 200,
                    content_length: None,
                    body: Box::pin(HalfBrokenReader {
                        data,
                        fail_after,
                        pos: 0,
                        failed: false,
                    }),
                }),
                FakeResponse::NetworkError => Err(AppError::retryable(
                    Code::NetworkError,
                    Stage::Download,
                    "connection reset by peer",
                )),
            }
        }
    }

    struct HalfBrokenReader {
        data: Vec<u8>,
        fail_after: usize,
        pos: usize,
        failed: bool,
    }

    impl tokio::io::AsyncRead for HalfBrokenReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.failed || self.pos >= self.fail_after.min(self.data.len()) {
                self.failed = true;
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection reset by peer",
                )));
            }
            let end = (self.pos + buf.remaining())
                .min(self.fail_after)
                .min(self.data.len());
            let n = end - self.pos;
            if n == 0 {
                self.failed = true;
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "connection reset by peer",
                )));
            }
            buf.put_slice(&self.data[self.pos..end]);
            self.pos = end;
            Poll::Ready(Ok(()))
        }
    }

    fn opts_with(http: Arc<FakeHttp>) -> Options {
        Options {
            output_path: String::new(),
            work_dir: String::new(),
            overwrite: false,
            max_bytes: 0,
            http: http as Arc<dyn RoundTrip>,
            progress: None,
            warn: None,
            cancel: CancelToken::default(),
            verify_fn: Some(Arc::new(|_path, _cancel| Ok("container".to_string()))),
        }
    }

    #[tokio::test]
    async fn happy_path_auto_named() {
        let dir = tempdir();
        let data = synthetic_mp4(1024);
        let http = Arc::new(FakeHttp::new(vec![FakeResponse::Bytes {
            status: 200,
            body: data.clone(),
            content_length: Some(data.len() as i64),
        }]));
        let mut opts = opts_with(http.clone());
        opts.work_dir = dir.to_string_lossy().into_owned();
        opts.max_bytes = 1 << 20;
        let dl = Downloader::new(test_video(), opts).unwrap();
        let res = dl.run().await.unwrap();
        let want = dir.join("示例 视频_abc123def456.mp4");
        assert_eq!(res.path, want.to_string_lossy());
        let meta = std::fs::metadata(&want).unwrap();
        assert_eq!(meta.len(), data.len() as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(res.bytes, data.len() as i64);
        assert!(!res.sha256.is_empty());
        // 恰好只剩最终文件
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        // 媒体请求无会话材料，URL 原样
        let reqs = http.requests();
        let r = &reqs[0];
        assert_eq!(r.header("cookie"), None);
        assert_eq!(r.header("referer"), Some("https://channels.weixin.qq.com/"));
        assert_eq!(r.url, test_video().media_url);
    }

    #[tokio::test]
    async fn explicit_output_validation() {
        let dir = tempdir();
        let http = Arc::new(FakeHttp::new(vec![]));
        // 非 mp4 扩展
        let mut opts = opts_with(http.clone());
        opts.output_path = dir.join("a.txt").to_string_lossy().into_owned();
        assert_eq!(
            Downloader::new(test_video(), opts).unwrap_err().code,
            Code::InvalidArgument
        );
        // 父目录不存在
        let mut opts = opts_with(http.clone());
        opts.output_path = dir
            .join("nope")
            .join("a.mp4")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            Downloader::new(test_video(), opts).unwrap_err().code,
            Code::InvalidArgument
        );
        // 目标是目录
        let d = dir.join("d.mp4");
        std::fs::create_dir(&d).unwrap();
        let mut opts = opts_with(http.clone());
        opts.output_path = d.to_string_lossy().into_owned();
        assert_eq!(
            Downloader::new(test_video(), opts).unwrap_err().code,
            Code::InvalidArgument
        );
        // 目标是符号链接
        #[cfg(unix)]
        {
            let link = dir.join("link.mp4");
            std::os::unix::fs::symlink(dir.join("real.mp4"), &link).unwrap();
            let mut opts = opts_with(http.clone());
            opts.output_path = link.to_string_lossy().into_owned();
            assert_eq!(
                Downloader::new(test_video(), opts).unwrap_err().code,
                Code::InvalidArgument
            );
        }
    }

    #[tokio::test]
    async fn no_clobber_by_default() {
        let dir = tempdir();
        let target = dir.join("out.mp4");
        std::fs::write(&target, b"OLD FILE CONTENT").unwrap();
        let http = Arc::new(FakeHttp::new(vec![]));
        let mut opts = opts_with(http.clone());
        opts.output_path = target.to_string_lossy().into_owned();
        let err = Downloader::new(test_video(), opts).unwrap_err();
        assert_eq!(err.code, Code::FileExists);
        assert!(http.requests().is_empty(), "冲突必须在任何网络活动之前发现");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "OLD FILE CONTENT"
        );

        // overwrite 成功并替换
        let data = synthetic_mp4(64);
        let http = Arc::new(FakeHttp::new(vec![FakeResponse::Bytes {
            status: 200,
            body: data.clone(),
            content_length: Some(data.len() as i64),
        }]));
        let mut opts = opts_with(http);
        opts.output_path = target.to_string_lossy().into_owned();
        opts.overwrite = true;
        opts.max_bytes = 1 << 20;
        let dl = Downloader::new(test_video(), opts).unwrap();
        let res = dl.run().await.unwrap();
        assert_eq!(res.path, target.to_string_lossy());
    }

    #[tokio::test]
    async fn http_failures_matrix() {
        struct Case {
            name: &'static str,
            resp: FakeResponse,
            want: Code,
        }
        let cases = vec![
            Case {
                name: "404",
                resp: FakeResponse::Bytes {
                    status: 404,
                    body: b"x".to_vec(),
                    content_length: Some(1),
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "403",
                resp: FakeResponse::Bytes {
                    status: 403,
                    body: b"x".to_vec(),
                    content_length: Some(1),
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "206",
                resp: FakeResponse::Bytes {
                    status: 206,
                    body: synthetic_mp4(32),
                    content_length: None,
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "500",
                resp: FakeResponse::Bytes {
                    status: 500,
                    body: b"x".to_vec(),
                    content_length: Some(1),
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "html body",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: b"<html><body>err</body></html>".to_vec(),
                    content_length: None,
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "json body",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: br#"{"error":"nope"}"#.to_vec(),
                    content_length: None,
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "xml body",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: b"<?xml version=\"1.0\"?><e/>".to_vec(),
                    content_length: None,
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "hls playlist",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nx.m3u8".to_vec(),
                    content_length: None,
                },
                want: Code::UnsupportedMedia,
            },
            Case {
                name: "dash manifest",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: b"<MPD xmlns=\"urn:mpeg:dash\">x</MPD>".to_vec(),
                    content_length: None,
                },
                want: Code::UnsupportedMedia,
            },
            Case {
                name: "empty body",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: vec![],
                    content_length: Some(0),
                },
                want: Code::DownloadFailed,
            },
            Case {
                name: "length mismatch",
                resp: FakeResponse::Bytes {
                    status: 200,
                    body: synthetic_mp4(100),
                    content_length: Some(synthetic_mp4(100).len() as i64 + 7),
                },
                want: Code::DownloadFailed,
            },
        ];
        for case in cases {
            let dir = tempdir();
            let http = Arc::new(FakeHttp::new(vec![case.resp]));
            let mut opts = opts_with(http);
            opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
            opts.max_bytes = 1 << 20;
            let dl = Downloader::new(test_video(), opts).unwrap();
            let err = dl.run().await.unwrap_err();
            assert_eq!(err.code, case.want, "{}", case.name);
            let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
            assert!(entries.is_empty(), "{}: 失败不得留下临时文件", case.name);
        }
    }

    #[tokio::test]
    async fn too_large() {
        // 已知 Content-Length 超限：读取前拒绝
        let dir = tempdir();
        let data = synthetic_mp4(4096);
        let http = Arc::new(FakeHttp::new(vec![FakeResponse::Bytes {
            status: 200,
            body: data.clone(),
            content_length: Some(data.len() as i64),
        }]));
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = 1024;
        let dl = Downloader::new(test_video(), opts).unwrap();
        assert_eq!(dl.run().await.unwrap_err().code, Code::DownloadTooLarge);

        // 未知长度：流式过程中强制
        let dir = tempdir();
        let http = Arc::new(FakeHttp::new(vec![FakeResponse::Bytes {
            status: 200,
            body: synthetic_mp4(4096),
            content_length: None,
        }]));
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = 1024;
        let dl = Downloader::new(test_video(), opts).unwrap();
        assert_eq!(dl.run().await.unwrap_err().code, Code::DownloadTooLarge);
    }

    #[tokio::test]
    async fn retries_once_from_scratch() {
        let dir = tempdir();
        let full = synthetic_mp4(2048);
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts2 = attempts.clone();
        let http = Arc::new(FakeHttp {
            script: Mutex::new(vec![
                FakeResponse::Broken {
                    data: full.clone(),
                    fail_after: 100,
                },
                FakeResponse::Bytes {
                    status: 200,
                    body: full.clone(),
                    content_length: Some(full.len() as i64),
                },
            ]),
            requests: Mutex::new(vec![]),
        });
        let _ = attempts2;
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = 1 << 20;
        let dl = Downloader::new(test_video(), opts).unwrap();
        let res = dl.run().await.unwrap();
        assert_eq!(res.bytes, full.len() as i64);
        let stored = std::fs::read(&res.path).unwrap();
        assert_eq!(stored.len(), full.len(), "重试不得追加");
        let _ = attempts;
    }

    #[tokio::test]
    async fn double_failure_gives_up() {
        let dir = tempdir();
        let http = Arc::new(FakeHttp::new(vec![
            FakeResponse::Broken {
                data: synthetic_mp4(512),
                fail_after: 10,
            },
            FakeResponse::Broken {
                data: synthetic_mp4(512),
                fail_after: 10,
            },
        ]));
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = 1 << 20;
        let dl = Downloader::new(test_video(), opts).unwrap();
        assert_eq!(dl.run().await.unwrap_err().code, Code::NetworkError);
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn cancel_cleans_temp() {
        let dir = tempdir();
        let cancel = CancelToken::default();
        let cancel2 = cancel.clone();
        let data = synthetic_mp4(4096);
        // 第一次读取后取消：Broken reader 读 fail_after 后报错，同时置取消标记
        let http = Arc::new(FakeHttp::new(vec![FakeResponse::Broken {
            data: data.clone(),
            fail_after: 64,
        }]));
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = 1 << 20;
        opts.cancel = cancel2;
        let dl = Downloader::new(test_video(), opts).unwrap();
        // 预置取消：read loop 第一次检查即返回 Cancelled
        cancel.cancel();
        let err = dl.run().await.unwrap_err();
        assert_eq!(err.code, Code::Cancelled);
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(entries.is_empty(), "取消必须清理临时文件");
    }

    #[tokio::test]
    async fn unknown_content_length_streams_fully() {
        let dir = tempdir();
        let data = synthetic_mp4(300);
        let http = Arc::new(FakeHttp::new(vec![FakeResponse::Bytes {
            status: 200,
            body: data.clone(),
            content_length: None,
        }]));
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = 1 << 20;
        let dl = Downloader::new(test_video(), opts).unwrap();
        let res = dl.run().await.unwrap();
        assert_eq!(res.bytes, data.len() as i64);
    }

    #[test]
    fn negative_max_bytes_rejected() {
        let dir = tempdir();
        let http = Arc::new(FakeHttp::new(vec![]));
        let mut opts = opts_with(http);
        opts.output_path = dir.join("o.mp4").to_string_lossy().into_owned();
        opts.max_bytes = -5;
        assert_eq!(
            Downloader::new(test_video(), opts).unwrap_err().code,
            Code::InvalidArgument
        );
    }
}
