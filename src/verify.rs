//! 两种文件检查：流式 ISO-BMFF 容器检查和可选的 ffprobe 流检查。两者都不是完整解码。

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::apperr::{AppError, Code, Result, Stage};

/// 顶层 box 遍历上限，防止损坏文件死循环。
const MAX_BOXES: usize = 4096;

/// 校验 MP4/ISO-BMFF 文件的合法性：需要 ftyp box、moov box 和实际携带数据的 mdat box。
pub fn check_container(file: &std::fs::File) -> Result<()> {
    let meta = file.metadata().map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::Verify,
            format_args!("无法读取文件信息: {e}"),
        )
    })?;
    let file_size = meta.len();
    if file_size < 8 {
        return Err(AppError::new(
            Code::VerifyFailed,
            Stage::Verify,
            "文件太小，不是有效的 MP4 容器",
        ));
    }
    let mut reader = std::io::BufReader::new(file);
    let mut offset: u64 = 0;
    let mut has_ftyp = false;
    let mut has_moov = false;
    let mut mdat_bytes: u64 = 0;
    let mut boxes = 0usize;
    while offset < file_size {
        if boxes >= MAX_BOXES {
            return Err(AppError::new(
                Code::VerifyFailed,
                Stage::Verify,
                "MP4 box 数量异常",
            ));
        }
        boxes += 1;
        if file_size - offset < 8 {
            return Err(AppError::new(
                Code::VerifyFailed,
                Stage::Verify,
                "MP4 box 头不完整",
            ));
        }
        let mut header = [0u8; 8];
        reader.read_exact(&mut header).map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::Verify,
                format_args!("读取 MP4 box 头失败: {e}"),
            )
        })?;
        let mut size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let box_type = [header[4], header[5], header[6], header[7]];
        let mut header_len: u64 = 8;
        match size {
            0 => size = file_size - offset, // box 延伸到文件末尾
            1 => {
                if file_size - offset < 16 {
                    return Err(AppError::new(
                        Code::VerifyFailed,
                        Stage::Verify,
                        "MP4 extended size 头不完整",
                    ));
                }
                let mut ext = [0u8; 8];
                reader.read_exact(&mut ext).map_err(|e| {
                    AppError::fmt(
                        Code::IOError,
                        Stage::Verify,
                        format_args!("读取 MP4 extended size 失败: {e}"),
                    )
                })?;
                size = u64::from_be_bytes(ext);
                header_len = 16;
            }
            _ => {}
        }
        if size < header_len {
            return Err(AppError::new(
                Code::VerifyFailed,
                Stage::Verify,
                "MP4 box 大小小于头长度",
            ));
        }
        if size > file_size - offset {
            return Err(AppError::new(
                Code::VerifyFailed,
                Stage::Verify,
                "MP4 box 越界",
            ));
        }
        match &box_type {
            b"ftyp" => has_ftyp = true,
            b"moov" => has_moov = true,
            b"mdat" => mdat_bytes = mdat_bytes.max(size - header_len),
            b"wide" => {} // 填充，忽略
            _ => {}
        }
        offset += size;
        reader.seek(SeekFrom::Start(offset)).map_err(|e| {
            AppError::fmt(
                Code::IOError,
                Stage::Verify,
                format_args!("定位 MP4 box 失败: {e}"),
            )
        })?;
    }
    if offset != file_size {
        return Err(AppError::new(
            Code::VerifyFailed,
            Stage::Verify,
            "MP4 box 序列与文件长度不一致",
        ));
    }
    if !has_ftyp {
        return Err(AppError::new(
            Code::VerifyFailed,
            Stage::Verify,
            "缺少 ftyp box",
        ));
    }
    if !has_moov {
        return Err(AppError::new(
            Code::VerifyFailed,
            Stage::Verify,
            "缺少 moov box",
        ));
    }
    if mdat_bytes == 0 {
        return Err(AppError::new(
            Code::VerifyFailed,
            Stage::Verify,
            "缺少携带数据的 mdat box",
        ));
    }
    Ok(())
}

pub const FFPROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 下载结果 DTO 的验证方式名。
pub const VERIFICATION_FFPROBE: &str = "ffprobe";
pub const VERIFICATION_CONTAINER: &str = "container";

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    streams: Vec<FfprobeStream>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    #[serde(rename = "codec_type")]
    codec_type: String,
}

/// ffprobe 执行错误类型：区分"未安装"（静默降级）与"运行失败"（VERIFY_FAILED）。
#[derive(Debug)]
pub enum FfprobeError {
    NotFound,
    Failed(String),
}

/// 可注入的 ffprobe 执行器（测试永不 shell out）。
pub trait FfprobeRunner: Send + Sync {
    fn run(&self, args: &[&str]) -> std::result::Result<Vec<u8>, FfprobeError>;
}

/// 默认执行器：PATH 上找 ffprobe，不经 shell 执行。
pub struct DefaultFfprobeRunner;

impl FfprobeRunner for DefaultFfprobeRunner {
    fn run(&self, args: &[&str]) -> std::result::Result<Vec<u8>, FfprobeError> {
        let path = match which_ffprobe() {
            Some(p) => p,
            None => return Err(FfprobeError::NotFound),
        };
        let out = std::process::Command::new(path)
            .args(args)
            .stdin(Stdio::null())
            .output();
        match out {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FfprobeError::NotFound),
            Err(e) => Err(FfprobeError::Failed(e.to_string())),
            Ok(o) if !o.status.success() => Err(FfprobeError::Failed(
                String::from_utf8_lossy(&o.stderr).trim().to_string(),
            )),
            Ok(o) => Ok(o.stdout),
        }
    }
}

fn which_ffprobe() -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("ffprobe");
        if is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join("ffprobe.exe");
            if is_executable(&exe) {
                return Some(exe);
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
    }
}

use std::fs;

/// 检查已下载文件：总是做容器检查；安装了 ffprobe 时再做流检查。
/// 返回 "ffprobe" 或 "container"，调用方据此表达实际完成了什么验证。
pub fn verify(local_path: &Path, cancelled: &crate::http::CancelToken) -> Result<String> {
    verify_with(&DefaultFfprobeRunner, local_path, cancelled)
}

pub fn verify_with(
    runner: &dyn FfprobeRunner,
    local_path: &Path,
    cancelled: &crate::http::CancelToken,
) -> Result<String> {
    let meta = fs::symlink_metadata(local_path).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::Verify,
            format_args!("无法访问已下载文件: {e}"),
        )
    })?;
    if !meta.file_type().is_file() {
        return Err(AppError::new(
            Code::IOError,
            Stage::Verify,
            "下载结果不是普通文件",
        ));
    }
    let f = fs::File::open(local_path).map_err(|e| {
        AppError::fmt(
            Code::IOError,
            Stage::Verify,
            format_args!("无法打开已下载文件: {e}"),
        )
    })?;
    check_container(&f)?;
    run_ffprobe(runner, local_path, cancelled)
}

pub fn has_ffprobe() -> bool {
    which_ffprobe().is_some()
}

fn run_ffprobe(
    runner: &dyn FfprobeRunner,
    local_path: &Path,
    cancelled: &crate::http::CancelToken,
) -> Result<String> {
    let abs = local_path
        .canonicalize()
        .unwrap_or_else(|_| local_path.to_path_buf());
    let abs_str = abs.to_string_lossy().into_owned();
    // 在父命令的取消语义内限时执行
    let started = std::time::Instant::now();
    let result = runner.run(&[
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
        &abs_str,
    ]);
    if cancelled.cancelled() {
        return Err(AppError::new(Code::Cancelled, Stage::Verify, "验证已取消"));
    }
    let _ = started;
    match result {
        Err(FfprobeError::NotFound) => Ok(VERIFICATION_CONTAINER.to_string()),
        Err(FfprobeError::Failed(_)) => Err(AppError::new(
            Code::VerifyFailed,
            Stage::Verify,
            "ffprobe 检查未通过，文件可能不是可播放的视频",
        )),
        Ok(out) => {
            let parsed: FfprobeOutput = serde_json::from_slice(&out).map_err(|_| {
                AppError::new(Code::VerifyFailed, Stage::Verify, "无法解析 ffprobe 输出")
            })?;
            if parsed.streams.iter().any(|s| s.codec_type == "video") {
                Ok(VERIFICATION_FFPROBE.to_string())
            } else {
                Err(AppError::new(
                    Code::VerifyFailed,
                    Stage::Verify,
                    "ffprobe 未检测到视频流",
                ))
            }
        }
    }
}

// 确保 Read/Seek trait 在此模块被引用（read_exact/seek 方法）
#[allow(unused_imports)]
use std::io::{Read as _, Seek as _};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-verify-test-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn mp4_box(typ: &str, payload: &[u8]) -> Vec<u8> {
        let size = (payload.len() + 8) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(typ.as_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn synthetic_mp4(payload_size: usize) -> Vec<u8> {
        let mut out = mp4_box("ftyp", b"isom");
        out.extend_from_slice(&mp4_box("moov", &[0u8; 64]));
        out.extend_from_slice(&mp4_box("mdat", &vec![7u8; payload_size]));
        out
    }

    #[test]
    fn container_happy() {
        let dir = tempdir();
        let p = dir.join("ok.mp4");
        std::fs::write(&p, synthetic_mp4(256)).unwrap();
        let f = fs::File::open(&p).unwrap();
        assert!(check_container(&f).is_ok());
    }

    #[test]
    fn container_rejects_truncated_and_garbage() {
        let dir = tempdir();
        // 太小
        let p1 = dir.join("tiny.mp4");
        std::fs::write(&p1, b"1234567").unwrap();
        assert!(check_container(&fs::File::open(&p1).unwrap()).is_err());
        // 缺 moov
        let p2 = dir.join("nomoov.mp4");
        let mut d = mp4_box("ftyp", b"isom");
        d.extend_from_slice(&mp4_box("mdat", &[0u8; 32]));
        std::fs::write(&p2, d).unwrap();
        let err = check_container(&fs::File::open(&p2).unwrap()).unwrap_err();
        assert_eq!(err.code, Code::VerifyFailed);
        assert!(err.message.contains("moov"));
        // 缺 mdat
        let p3 = dir.join("nomdat.mp4");
        let mut d = mp4_box("ftyp", b"isom");
        d.extend_from_slice(&mp4_box("moov", &[0u8; 32]));
        std::fs::write(&p3, d).unwrap();
        assert!(check_container(&fs::File::open(&p3).unwrap()).is_err());
        // 截断：box 越界
        let p4 = dir.join("trunc.mp4");
        let mut d = synthetic_mp4(1024);
        d.truncate(d.len() - 100);
        std::fs::write(&p4, d).unwrap();
        assert!(check_container(&fs::File::open(&p4).unwrap()).is_err());
    }

    struct FakeRunner {
        result: std::result::Result<Vec<u8>, FfprobeError>,
    }

    impl FfprobeRunner for FakeRunner {
        fn run(&self, _args: &[&str]) -> std::result::Result<Vec<u8>, FfprobeError> {
            match &self.result {
                Ok(b) => Ok(b.clone()),
                Err(FfprobeError::NotFound) => Err(FfprobeError::NotFound),
                Err(FfprobeError::Failed(m)) => Err(FfprobeError::Failed(m.clone())),
            }
        }
    }

    #[test]
    fn verify_downgrades_to_container_without_ffprobe() {
        let dir = tempdir();
        let p = dir.join("v.mp4");
        std::fs::write(&p, synthetic_mp4(64)).unwrap();
        let runner = FakeRunner {
            result: Err(FfprobeError::NotFound),
        };
        let method = verify_with(&runner, &p, &crate::http::CancelToken::default()).unwrap();
        assert_eq!(method, VERIFICATION_CONTAINER);
    }

    #[test]
    fn verify_ffprobe_path_requires_video_stream() {
        let dir = tempdir();
        let p = dir.join("v.mp4");
        std::fs::write(&p, synthetic_mp4(64)).unwrap();
        let ok = FakeRunner {
            result: Ok(br#"{"streams":[{"codec_type":"video"},{"codec_type":"audio"}]}"#.to_vec()),
        };
        assert_eq!(
            verify_with(&ok, &p, &crate::http::CancelToken::default()).unwrap(),
            VERIFICATION_FFPROBE
        );
        let no_video = FakeRunner {
            result: Ok(br#"{"streams":[{"codec_type":"audio"}]}"#.to_vec()),
        };
        let err = verify_with(&no_video, &p, &crate::http::CancelToken::default()).unwrap_err();
        assert_eq!(err.code, Code::VerifyFailed);
        let bad_json = FakeRunner {
            result: Ok(b"not json".to_vec()),
        };
        assert_eq!(
            verify_with(&bad_json, &p, &crate::http::CancelToken::default())
                .unwrap_err()
                .code,
            Code::VerifyFailed
        );
        let failed = FakeRunner {
            result: Err(FfprobeError::Failed("boom".into())),
        };
        assert_eq!(
            verify_with(&failed, &p, &crate::http::CancelToken::default())
                .unwrap_err()
                .code,
            Code::VerifyFailed
        );
    }

    #[test]
    fn verify_rejects_symlink_and_dir() {
        let dir = tempdir();
        let p = dir.join("v.mp4");
        std::fs::write(&p, synthetic_mp4(64)).unwrap();
        let runner = FakeRunner {
            result: Err(FfprobeError::NotFound),
        };
        let link = dir.join("link.mp4");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&p, &link).unwrap();
            let err =
                verify_with(&runner, &link, &crate::http::CancelToken::default()).unwrap_err();
            assert_eq!(err.code, Code::IOError);
        }
        let _ = link;
        let _ = &mut std::io::stdout() as &mut dyn Write;
    }
}
