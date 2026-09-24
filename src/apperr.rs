//! 稳定的错误模型：机器可读错误码、处理阶段、安全的人类可读消息和稳定的进程退出码。
//!
//! 消息必须保持可打印的安全性：不得包含 Cookie、会话头、generalToken、带签名的媒体
//! URL 或上游原始响应体。包装的根本原因永不渲染。

/// 机器可读错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    InternalError,
    InvalidArgument,
    AuthRequired,
    InvalidCredentials,
    NetworkError,
    Timeout,
    UpstreamError,
    AccessDenied,
    RateLimited,
    VideoUnavailable,
    NoMedia,
    UnsupportedMedia,
    DownloadFailed,
    DownloadTooLarge,
    FileExists,
    IOError,
    VerifyFailed,
    SchemaChanged,
    LoginDependencyMissing,
    LoginBrowserFailed,
    InteractiveRequired,
    LoginCaptureFailed,
    AuthBusy,
    Cancelled,
    // v2 新增（PRD §8）
    SessionExpired,
    PublishRejected,
    RecoveryFailed,
    ScheduleInvalid,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::InternalError => "INTERNAL_ERROR",
            Code::InvalidArgument => "INVALID_ARGUMENT",
            Code::AuthRequired => "AUTH_REQUIRED",
            Code::InvalidCredentials => "INVALID_CREDENTIALS",
            Code::NetworkError => "NETWORK_ERROR",
            Code::Timeout => "TIMEOUT",
            Code::UpstreamError => "UPSTREAM_ERROR",
            Code::AccessDenied => "ACCESS_DENIED",
            Code::RateLimited => "RATE_LIMITED",
            Code::VideoUnavailable => "VIDEO_UNAVAILABLE",
            Code::NoMedia => "NO_MEDIA",
            Code::UnsupportedMedia => "UNSUPPORTED_MEDIA",
            Code::DownloadFailed => "DOWNLOAD_FAILED",
            Code::DownloadTooLarge => "DOWNLOAD_TOO_LARGE",
            Code::FileExists => "FILE_EXISTS",
            Code::IOError => "IO_ERROR",
            Code::VerifyFailed => "VERIFY_FAILED",
            Code::SchemaChanged => "SCHEMA_CHANGED",
            Code::LoginDependencyMissing => "LOGIN_DEPENDENCY_MISSING",
            Code::LoginBrowserFailed => "LOGIN_BROWSER_FAILED",
            Code::InteractiveRequired => "INTERACTIVE_REQUIRED",
            Code::LoginCaptureFailed => "LOGIN_CAPTURE_FAILED",
            Code::AuthBusy => "AUTH_BUSY",
            Code::SessionExpired => "SESSION_EXPIRED",
            Code::PublishRejected => "PUBLISH_REJECTED",
            Code::RecoveryFailed => "RECOVERY_FAILED",
            Code::ScheduleInvalid => "SCHEDULE_INVALID",
            Code::Cancelled => "CANCELLED",
        }
    }

    /// 稳定的进程退出码。
    pub fn exit_code(self) -> i32 {
        match self {
            Code::InternalError => 1,
            Code::InvalidArgument => 2,
            Code::AuthRequired | Code::InvalidCredentials => 3,
            Code::NetworkError | Code::Timeout => 4,
            Code::UpstreamError | Code::AccessDenied | Code::RateLimited => 5,
            Code::VideoUnavailable | Code::NoMedia => 6,
            Code::UnsupportedMedia => 7,
            Code::DownloadFailed | Code::DownloadTooLarge => 8,
            Code::FileExists | Code::IOError => 9,
            Code::VerifyFailed => 10,
            Code::SchemaChanged => 11,
            Code::LoginDependencyMissing | Code::LoginBrowserFailed => 12,
            Code::InteractiveRequired => 13,
            Code::LoginCaptureFailed | Code::AuthBusy => 14,
            Code::SessionExpired => 15,
            Code::PublishRejected => 16,
            Code::RecoveryFailed => 17,
            Code::ScheduleInvalid => 18,
            Code::Cancelled => 130,
        }
    }
}

/// 出错阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Arguments,
    Credentials,
    LoginDeps,
    LoginBrowser,
    LoginCapture,
    LoginCommit,
    ParseShare,
    FetchFeed,
    SelectMedia,
    Download,
    Verify,
    Commit,
    // v2 发布流水线
    SessionLoad,
    Navigate,
    Upload,
    Metadata,
    Cover,
    Declaration,
    Schedule,
    Submit,
    Recovery,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Arguments => "arguments",
            Stage::Credentials => "credentials",
            Stage::LoginDeps => "login_dependencies",
            Stage::LoginBrowser => "login_browser",
            Stage::LoginCapture => "login_capture",
            Stage::LoginCommit => "login_commit",
            Stage::ParseShare => "parse_share",
            Stage::FetchFeed => "fetch_feed",
            Stage::SelectMedia => "select_media",
            Stage::Download => "download",
            Stage::Verify => "verify",
            Stage::Commit => "commit",
            Stage::SessionLoad => "session_load",
            Stage::Navigate => "navigate",
            Stage::Upload => "upload",
            Stage::Metadata => "metadata",
            Stage::Cover => "cover",
            Stage::Declaration => "declaration",
            Stage::Schedule => "schedule",
            Stage::Submit => "submit",
            Stage::Recovery => "recovery",
        }
    }
}

/// 内部包之间传递的错误类型。
#[derive(Debug, Clone)]
pub struct AppError {
    pub code: Code,
    pub stage: Stage,
    pub message: String,
    pub retryable: bool,
}

impl AppError {
    pub fn new(code: Code, stage: Stage, msg: impl Into<String>) -> AppError {
        AppError {
            code,
            stage,
            message: msg.into(),
            retryable: false,
        }
    }

    pub fn retryable(code: Code, stage: Stage, msg: impl Into<String>) -> AppError {
        AppError {
            code,
            stage,
            message: msg.into(),
            retryable: true,
        }
    }

    pub fn fmt(code: Code, stage: Stage, args: std::fmt::Arguments) -> AppError {
        AppError::new(code, stage, format!("{args}"))
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}): {}",
            self.code.as_str(),
            self.stage.as_str(),
            self.message
        )
    }
}

impl std::error::Error for AppError {}

/// 结果别名。
pub type Result<T> = std::result::Result<T, AppError>;

/// 将任意错误归入错误模型；已是 AppError 的原样返回。
pub fn from(err: impl Into<BoxError>) -> AppError {
    let err = err.into();
    match err.downcast::<AppError>() {
        Ok(ae) => *ae,
        Err(other) => {
            let msg = format!("{other}");
            if msg.contains("operation timed out") {
                AppError::new(Code::Timeout, Stage::Arguments, "操作超时")
            } else {
                AppError::new(Code::InternalError, Stage::Arguments, "未预期的内部错误")
            }
        }
    }
}

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// 方便的 ? 转换：任意实现 Error 的类型经此包装为内部错误，除非它本就是 AppError。
pub fn wrap_internal(stage: Stage, msg: impl Into<String>) -> impl FnOnce(BoxError) -> AppError {
    let msg = msg.into();
    move |e| {
        let _ = e;
        AppError::new(Code::InternalError, stage, msg)
    }
}
