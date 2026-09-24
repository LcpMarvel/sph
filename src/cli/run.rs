//! 命令调度、参数处理、输出契约（纯文本 vs 单个 JSON 对象）与退出码映射。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use time::OffsetDateTime;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::auth;
use crate::cli::args::{parse_args, Command};
use crate::cli::output::{self, ProgressPrinter};
use crate::download::{self, Downloader, Options as DownloadOptions};
use crate::http::{CancelToken, RoundTrip};
use crate::login;
use crate::media;
use crate::publish;
use crate::session;
use crate::upstream::Client;

/// 由 Release 构建经环境注入的版本号；源码构建显示 dev。
pub fn version() -> &'static str {
    option_env!("SPH_VERSION").unwrap_or("dev")
}

const DEFAULT_INSPECT_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// 可注入依赖的集合：测试永不触碰网络、Node 或浏览器。
pub struct Deps {
    pub config_dir: Box<dyn Fn() -> PathBuf + Send + Sync>,
    pub interactive: Box<dyn Fn() -> bool + Send + Sync>,
    /// 让测试把解析链指向假服务器。
    pub upstream_factory: Box<dyn Fn() -> Client + Send + Sync>,
    /// 让测试伪造媒体服务器。
    pub media_factory: Box<dyn Fn() -> Arc<dyn RoundTrip> + Send + Sync>,
    pub cancel: CancelToken,
}

impl Deps {
    pub fn production() -> Deps {
        Deps {
            config_dir: Box::new(auth::default_config_dir),
            interactive: Box::new(output::stdin_is_terminal),
            upstream_factory: Box::new(|| {
                Client::new(Arc::new(crate::http::ReqwestTransport::api()))
            }),
            media_factory: Box::new(|| Arc::new(crate::http::ReqwestTransport::media())),
            cancel: CancelToken::default(),
        }
    }
}

/// stdin 的共享读取端（--stdin 链接、auth import、login 行读取共用）。
pub struct SharedStdin(pub Arc<Mutex<dyn Read + Send>>);

impl login::LineSource for SharedStdin {
    fn read_line(&mut self) -> Option<String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1];
        let mut reader = match self.0.lock() {
            Ok(r) => r,
            Err(_) => return None,
        };
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return None,
                Ok(_) => {
                    if chunk[0] == b'\n' {
                        break;
                    }
                    buf.push(chunk[0]);
                    if buf.len() > 8192 {
                        break;
                    }
                }
                Err(_) => return None,
            }
        }
        let line = String::from_utf8_lossy(&buf)
            .trim_end_matches('\r')
            .to_string();
        Some(line)
    }
}

/// 执行一次命令调用并返回进程退出码。
/// stdout 恰好收到一个最终负载（--json 模式下一个 JSON 对象）；其余全走 stderr。
pub type SharedWriter = Arc<Mutex<dyn Write + Send>>;

pub async fn run(
    argv: &[String],
    stdin: Arc<Mutex<dyn Read + Send>>,
    stdout: &mut dyn Write,
    stderr: SharedWriter,
    deps: &Deps,
) -> i32 {
    let cmd = match parse_args(argv) {
        Ok(c) => c,
        Err(e) => {
            let json = argv.iter().any(|a| a == "--json");
            let mut guard = stderr.lock().unwrap_or_else(|e| e.into_inner());
            return output::fail(&e, json, stdout, &mut *guard);
        }
    };
    let json_mode = cmd.bool_flag("json");
    let result = dispatch(&cmd, stdin, stdout, &stderr, deps).await;
    let err = match result {
        Ok(()) => return 0,
        Err(e) => e,
    };
    let err = if deps.cancel.cancelled() && err.code != Code::Cancelled {
        // 外层取消（Ctrl+C）优先于任何被包装的原因
        AppError::new(Code::Cancelled, err.stage, "操作已取消")
    } else {
        err
    };
    let mut guard = stderr.lock().unwrap_or_else(|e| e.into_inner());
    output::fail(&err, json_mode, stdout, &mut *guard)
}

async fn dispatch(
    cmd: &Command,
    stdin: Arc<Mutex<dyn Read + Send>>,
    stdout: &mut dyn Write,
    stderr: &SharedWriter,
    deps: &Deps,
) -> Result<()> {
    match cmd.command.as_str() {
        "help" => {
            stdout
                .write_all(output::HELP_TEXT.as_bytes())
                .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出帮助"))?;
            Ok(())
        }
        "version" => {
            writeln!(stdout, "sph {}", version())
                .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
            Ok(())
        }
        "login" => run_login(cmd, stdin, stderr.clone(), deps).await,
        "auth status" => run_auth_status(stdout, deps),
        "auth import" => run_auth_import(cmd, stdin, stdout, deps),
        "logout" | "auth clear" => run_logout(cmd, stdout, deps).await,
        "publish" => run_publish(cmd, stdout, stderr, deps).await,
        "accounts" => run_accounts(stdout, deps),
        "inspect" => run_inspect(cmd, stdin, stdout, stderr, deps).await,
        "download" => run_download(cmd, stdin, stdout, stderr, deps).await,
        other => Err(AppError::fmt(
            Code::InvalidArgument,
            Stage::Arguments,
            format_args!("未知命令：{other}"),
        )),
    }
}

// --- URL 输入处理 ---------------------------------------------------

/// 从位置参数或 --stdin 取得单个分享链接。
fn input_url(cmd: &Command, stdin: &Arc<Mutex<dyn Read + Send>>) -> Result<String> {
    let use_stdin = cmd.bool_flag("stdin");
    if use_stdin && !cmd.positionals.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "--stdin 与位置参数 URL 不能同时使用",
        ));
    }
    if use_stdin {
        let mut data = Vec::new();
        {
            let mut reader = stdin.lock().map_err(|_| {
                AppError::new(Code::InvalidArgument, Stage::Arguments, "读取 stdin 失败")
            })?;
            let mut limited = (&mut *reader).take((64 << 10) + 1);
            limited.read_to_end(&mut data).map_err(|_| {
                AppError::new(Code::InvalidArgument, Stage::Arguments, "读取 stdin 失败")
            })?;
        }
        if data.len() > 64 << 10 {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "stdin 输入超过 64 KiB 上限",
            ));
        }
        let text = String::from_utf8_lossy(&data).trim().to_string();
        if text.is_empty() {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "stdin 输入为空",
            ));
        }
        if text.contains(['\n', '\r']) {
            return Err(AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "stdin 输入包含多个链接",
            ));
        }
        return Ok(text);
    }
    match cmd.positionals.len() {
        0 => Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "缺少分享链接参数",
        )),
        1 => Ok(cmd.positionals[0].clone()),
        _ => Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "一次只支持一个链接",
        )),
    }
}

/// 解析 --timeout：Go time.ParseDuration 语义的子集（ns/us/ms/s/m/h 组合）。
pub fn parse_duration(raw: &str) -> Option<Duration> {
    if raw.is_empty() {
        return None;
    }
    let mut total = Duration::ZERO;
    let mut num = String::new();
    let mut chars = raw.chars().peekable();
    let mut negative = false;
    if chars.peek() == Some(&'-') {
        negative = true;
        chars.next();
    }
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let value: f64 = num.parse().ok()?;
        num.clear();
        let mult = match c {
            'n' => {
                if chars.next() != Some('s') {
                    return None;
                }
                1e-9
            }
            'u' | 'µ' => 1e-6,
            'm' => {
                if chars.peek() == Some(&'s') {
                    chars.next();
                    1e-3
                } else {
                    60.0
                }
            }
            's' => 1.0,
            'h' => 3600.0,
            _ => return None,
        };
        total += Duration::from_secs_f64(value * mult);
    }
    if !num.is_empty() {
        // 末尾缺单位
        return None;
    }
    if negative {
        total = Duration::ZERO;
    }
    Some(total)
}

fn parse_timeout_flag(cmd: &Command, def: Duration) -> Result<Duration> {
    let Some(raw) = cmd.flag("timeout") else {
        return Ok(def);
    };
    match parse_duration(raw) {
        Some(d) if !d.is_zero() => Ok(d),
        _ => Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "--timeout 必须是正的时长（如 60s、5m）",
        )),
    }
}

// --- inspect / download ------------------------------------------------

fn load_credentials(deps: &Deps) -> Result<auth::Credentials> {
    let store = auth::Store::new((deps.config_dir)())?;
    store.load()
}

async fn with_command_timeout<T>(
    timeout: Duration,
    cancel: CancelToken,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => {
            if cancel.cancelled() {
                Err(AppError::new(
                    Code::Cancelled,
                    Stage::Arguments,
                    "操作已取消",
                ))
            } else {
                Err(AppError::new(Code::Timeout, Stage::Arguments, "操作超时"))
            }
        }
    }
}

async fn run_inspect(
    cmd: &Command,
    stdin: Arc<Mutex<dyn Read + Send>>,
    stdout: &mut dyn Write,
    _stderr: &SharedWriter,
    deps: &Deps,
) -> Result<()> {
    let raw_url = input_url(cmd, &stdin)?;
    let timeout = parse_timeout_flag(cmd, DEFAULT_INSPECT_TIMEOUT)?;
    let creds = load_credentials(deps)?;
    let cancel = deps.cancel.clone();
    let result = with_command_timeout(timeout, cancel.clone(), async {
        let mut client = (deps.upstream_factory)();
        client.cancel = cancel;
        client.resolve(&raw_url, &creds).await
    })
    .await;
    let video = match result {
        Ok(v) => v,
        Err(e) => {
            return Err(e);
        }
    };
    let dto = video.inspect();
    if cmd.bool_flag("json") {
        let env =
            output::success_envelope("inspect", serde_json::to_value(&dto).unwrap_or(json!({})));
        return output::write_json(stdout, &env);
    }
    writeln!(stdout, "本地标识: {}", dto.local_id)
        .and_then(|_| writeln!(stdout, "标题: {}", dto.title))
        .and_then(|_| writeln!(stdout, "作者: {}", dto.author))
        .and_then(|_| writeln!(stdout, "媒体来源: {}", dto.media_source))
        .and_then(|_| writeln!(stdout, "编码提示: {}", dto.codec_hint))
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    Ok(())
}

async fn run_download(
    cmd: &Command,
    stdin: Arc<Mutex<dyn Read + Send>>,
    stdout: &mut dyn Write,
    stderr: &SharedWriter,
    deps: &Deps,
) -> Result<()> {
    let raw_url = input_url(cmd, &stdin)?;
    let timeout = parse_timeout_flag(cmd, DEFAULT_DOWNLOAD_TIMEOUT)?;
    let mut max_bytes = download::DEFAULT_MAX_BYTES;
    if let Some(raw) = cmd.flag("max-bytes") {
        match raw.parse::<i64>() {
            Ok(v) if v > 0 => max_bytes = v,
            _ => {
                return Err(AppError::new(
                    Code::InvalidArgument,
                    Stage::Arguments,
                    "--max-bytes 必须是正整数",
                ))
            }
        }
    }
    let creds = load_credentials(deps)?;
    let cancel = deps.cancel.clone();
    let video = with_command_timeout(timeout, cancel.clone(), async {
        let mut client = (deps.upstream_factory)();
        client.cancel = cancel;
        client.resolve(&raw_url, &creds).await
    })
    .await?;

    let output_path = cmd.flag("output").unwrap_or("").to_string();
    let stderr_progress: SharedWriter = stderr.clone();
    let printer = Arc::new(Mutex::new(ProgressPrinter::new(stderr_progress.clone())));
    let printer_for_cb = printer.clone();
    let warn_stderr = stderr_progress;
    let opts = DownloadOptions {
        output_path,
        work_dir: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        overwrite: cmd.bool_flag("overwrite"),
        max_bytes,
        http: (deps.media_factory)(),
        progress: Some(Box::new(move |sent, total| {
            if let Ok(mut p) = printer_for_cb.lock() {
                p.call(sent, total);
            }
        })),
        warn: Some(Box::new(move |msg| {
            if let Ok(mut g) = warn_stderr.lock() {
                let _ = writeln!(g, "警告：{msg}");
            }
        })),
        cancel: deps.cancel.clone(),
        verify_fn: None,
    };
    let dl = Downloader::new(video, opts)?;
    let result = with_command_timeout(timeout, deps.cancel.clone(), dl.run()).await?;
    if result.verification == crate::verify::VERIFICATION_CONTAINER {
        if let Ok(mut g) = stderr.lock() {
            let _ = writeln!(
                g,
                "提示：未安装 ffprobe，仅完成容器基础检查，尚未验证视频流与播放。"
            );
        }
    }
    if cmd.bool_flag("json") {
        let env = output::success_envelope(
            "download",
            serde_json::to_value(&result).map_err(|_| {
                AppError::new(Code::InternalError, Stage::Arguments, "结果序列化失败")
            })?,
        );
        return output::write_json(stdout, &env);
    }
    writeln!(stdout, "{}", result.path)
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    if let Ok(mut g) = stderr.lock() {
        let _ = writeln!(
            g,
            "已保存 {} 字节（SHA-256 {}，验证方式 {}）",
            result.bytes, result.sha256, result.verification
        );
    }
    Ok(())
}

// --- auth 命令 -----------------------------------------------------------

fn run_auth_status(stdout: &mut dyn Write, deps: &Deps) -> Result<()> {
    let store = auth::Store::new((deps.config_dir)())?;
    if !store.exists() {
        writeln!(stdout, "状态: 未配置凭证")
            .and_then(|_| writeln!(stdout, "配置目录: {}", store.dir.display()))
            .and_then(|_| writeln!(stdout, "如需登录请执行: sph login"))
            .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
        return Ok(());
    }
    writeln!(stdout, "状态: 已保存本地凭证")
        .and_then(|_| writeln!(stdout, "配置目录: {}", store.dir.display()))
        .and_then(|_| writeln!(stdout, "凭证文件: {}", store.path().display()))
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    let creds = match store.load() {
        Ok(c) => c,
        Err(_) => {
            writeln!(stdout, "读取详情失败: 凭证文件存在但未通过校验")
                .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
            return Ok(());
        }
    };
    let format_rfc3339 = |t: time::OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| t.to_string())
    };
    writeln!(stdout, "来源: {}", creds.source.as_str())
        .and_then(|_| writeln!(stdout, "保存时间: {}", format_rfc3339(creds.saved_at)))
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    match creds.verified_at {
        Some(t) => writeln!(stdout, "上次验证: {}", format_rfc3339(t)),
        None => writeln!(stdout, "上次验证: 未验证"),
    }
    .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    if creds.yuanbao_headers.is_empty() {
        writeln!(stdout, "额外请求头: 无")
            .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    } else {
        let names: Vec<&str> = creds.yuanbao_headers.keys().map(|s| s.as_str()).collect();
        writeln!(stdout, "额外请求头: {}", names.join(", "))
            .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    }
    writeln!(stdout, "（以上仅为上次验证时间，不代表当前必然有效）")
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    Ok(())
}

fn run_auth_import(
    cmd: &Command,
    stdin: Arc<Mutex<dyn Read + Send>>,
    stdout: &mut dyn Write,
    deps: &Deps,
) -> Result<()> {
    if !cmd.bool_flag("stdin") {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "auth import 需要 --stdin（例如 pbpaste | sph auth import --stdin）",
        ));
    }
    if !cmd.positionals.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "auth import 不接受位置参数",
        ));
    }
    let mut data = Vec::new();
    {
        let mut reader = stdin.lock().map_err(|_| {
            AppError::new(Code::InvalidArgument, Stage::Arguments, "读取 stdin 失败")
        })?;
        let mut limited = (&mut *reader).take((64 << 10) + 1);
        limited.read_to_end(&mut data).map_err(|_| {
            AppError::new(Code::InvalidArgument, Stage::Arguments, "读取 stdin 失败")
        })?;
    }
    if data.len() > 64 << 10 {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "读取 stdin 失败或超过 64 KiB 上限",
        ));
    }
    let cookie = auth::parse_cookie_import(&String::from_utf8_lossy(&data))?;
    let mut headers = std::collections::BTreeMap::new();
    if let Some(hf) = cmd.flag("headers-file") {
        let raw = std::fs::read(hf).map_err(|_| {
            AppError::new(
                Code::InvalidArgument,
                Stage::Arguments,
                "无法读取额外请求头文件",
            )
        })?;
        headers = auth::parse_headers_file(&raw)?;
    }
    let store = auth::Store::new((deps.config_dir)())?;
    let lock = store.acquire_lock()?;
    let creds = auth::Credentials {
        saved_at: time::OffsetDateTime::now_utc(),
        source: auth::Source::ManualImport,
        verified_at: None, // 导入只证明格式，不证明有效性
        cookie,
        yuanbao_headers: headers,
    };
    store.save(&creds)?;
    lock.release()?;
    writeln!(stdout, "已导入，尚未验证；首次使用时会通过解析链验证。")
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    Ok(())
}

async fn run_logout(cmd: &Command, stdout: &mut dyn Write, deps: &Deps) -> Result<()> {
    if cmd.bool_flag("assistant") {
        // 清视频号助手会话（持久 profile + 档案）
        let config_dir = (deps.config_dir)();
        let account_name = session::DEFAULT_ACCOUNT;
        let dir = session::AccountDir::new(&session::accounts_root(&config_dir), account_name);
        if !dir.root.exists() {
            writeln!(stdout, "账号 {account_name} 没有已保存的助手会话。")
                .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
            return Ok(());
        }
        dir.remove_session()?;
        writeln!(
            stdout,
            "已清除账号 {account_name} 的助手会话（本地 profile，不影响手机端登录）。"
        )
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
        return Ok(());
    }
    // v1 契约：清元宝下载凭证
    let store = auth::Store::new((deps.config_dir)())?;
    let lock = store.acquire_lock()?;
    store.clear()?;
    lock.release()?;
    writeln!(stdout, "已清除本工具保存的本地凭证（不影响浏览器登录）。")
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    Ok(())
}

// --- login ----------------------------------------------------------------

async fn run_login(
    cmd: &Command,
    stdin: Arc<Mutex<dyn Read + Send>>,
    stderr: SharedWriter,
    deps: &Deps,
) -> Result<()> {
    if cmd.bool_flag("json") {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "login 不支持 --json",
        ));
    }
    if !cmd.positionals.is_empty() {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "login 不接受位置参数",
        ));
    }
    let timeout = parse_timeout_flag(cmd, login::DEFAULT_TIMEOUT)?;
    if cmd.bool_flag("yuanbao") {
        // v1 元宝凭证路径（下载域）
        let store = auth::Store::new((deps.config_dir)())?;
        let opts = login::Options {
            timeout,
            stderr,
            launch: None,
            now: None,
            stdin: Some(Box::new(SharedStdin(stdin))),
            cancel: Some(deps.cancel.clone()),
        };
        return login::run(&store, (deps.interactive)(), opts).await;
    }
    // 默认：视频号助手持久会话
    let config_dir = (deps.config_dir)();
    let opts = session::LoginOptions {
        timeout,
        stderr,
        launch: None,
        now: None,
    };
    session::login_assistant(
        &config_dir,
        session::DEFAULT_ACCOUNT,
        (deps.interactive)(),
        opts,
    )
    .await
}

// --- publish / accounts ----------------------------------------------------

async fn run_publish(
    cmd: &Command,
    stdout: &mut dyn Write,
    stderr: &SharedWriter,
    deps: &Deps,
) -> Result<()> {
    let json_mode = cmd.bool_flag("json");
    // 位置参数：视频文件
    if cmd.positionals.len() != 1 {
        return Err(AppError::new(
            Code::InvalidArgument,
            Stage::Arguments,
            "publish 需要一个视频文件路径",
        ));
    }
    let video = PathBuf::from(&cmd.positionals[0]);
    let timeout = parse_timeout_flag(cmd, Duration::from_secs(10 * 60))?;
    let opts = publish::default_options(
        video,
        cmd.flag("title").unwrap_or("").to_string(),
        cmd.flag("description").unwrap_or("").to_string(),
        parse_tags(cmd.flag("tags")),
        cmd.flag("cover").map(PathBuf::from),
        cmd.bool_flag("dry-run"),
        cmd.bool_flag("headed"),
        timeout,
        deps.cancel.clone(),
        stderr.clone(),
    );
    let account = cmd
        .flag("account")
        .unwrap_or(session::DEFAULT_ACCOUNT)
        .to_string();
    publish::validate(&opts)?;
    let result = with_command_timeout(
        timeout,
        deps.cancel.clone(),
        publish::run(&(deps.config_dir)(), &account, opts),
    )
    .await?;
    if json_mode {
        let env = output::success_envelope(
            "publish",
            serde_json::to_value(&result).map_err(|_| {
                AppError::new(Code::InternalError, Stage::Arguments, "结果序列化失败")
            })?,
        );
        return output::write_json(stdout, &env);
    }
    writeln!(
        stdout,
        "{}（账号: {}，视频: {}）",
        if result.submitted {
            "发布已提交"
        } else {
            "dry-run 完成（未提交）"
        },
        result.account,
        result.video
    )
    .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    Ok(())
}

fn parse_tags(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split([',', '，'])
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

fn run_accounts(stdout: &mut dyn Write, deps: &Deps) -> Result<()> {
    let config_dir = (deps.config_dir)();
    let format_rfc3339 = |t: OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| t.to_string())
    };
    // 助手会话域
    let accounts = session::list_accounts(&config_dir);
    if accounts.is_empty() {
        writeln!(
            stdout,
            "助手会话: 未配置（执行 sph login 扫码登录视频号助手）"
        )
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    } else {
        writeln!(stdout, "助手会话:")
            .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
        for name in accounts {
            let dir = session::AccountDir::new(&session::accounts_root(&config_dir), &name);
            let account = dir.load().unwrap_or_default();
            match account.assistant_logged_in_at {
                Some(t) => writeln!(stdout, "  - {name}: 上次登录 {}", format_rfc3339(t))
                    .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?,
                None => writeln!(stdout, "  - {name}: （档案存在但无登录记录）")
                    .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?,
            }
        }
    }
    // 元宝凭证域（下载）
    let store = auth::Store::new(&config_dir)?;
    if store.exists() {
        match store.load() {
            Ok(creds) => writeln!(
                stdout,
                "下载凭证: 已保存（来源 {}，保存时间 {}）",
                creds.source.as_str(),
                format_rfc3339(creds.saved_at)
            )
            .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?,
            Err(_) => writeln!(stdout, "下载凭证: 文件存在但未通过校验")
                .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?,
        }
    } else {
        writeln!(
            stdout,
            "下载凭证: 未配置（执行 sph login --yuanbao 登录元宝）"
        )
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    }
    Ok(())
}

// 编译期引用，避免未使用告警漂移
#[allow(dead_code)]
fn _ref(p: &media::ResolvedVideo) -> &media::ResolvedVideo {
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{RawRequest, RawResponse, RoundTrip};
    use std::sync::Mutex as StdMutex;

    const SENTINEL_COOKIE: &str = "DO_NOT_LEAK_COOKIE_123";
    const SENTINEL_TOKEN: &str = "DO_NOT_LEAK_TOKEN_456";
    const SENTINEL_MEDIA: &str = "DO_NOT_LEAK_MEDIA_QUERY_789";
    const SENTINEL_HEADER: &str = "DO_NOT_LEAK_HEADER_321";

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-cli-test-{:016x}", {
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

    fn synthetic_mp4() -> Vec<u8> {
        let mut out = mp4_box("ftyp", b"isom");
        out.extend_from_slice(&mp4_box("moov", &[0u8; 32]));
        out.extend_from_slice(&mp4_box("mdat", &vec![0u8; 256]));
        out
    }

    /// 把 MP4 的最后一个 mdat box 改成 size=0（延伸到 EOF），使后续追加的字节被容器检查视为 mdat 的一部分。
    fn patch_mdat_to_eof(mut mp4: Vec<u8>) -> Vec<u8> {
        let mut offset = 0usize;
        let len = mp4.len();
        while offset + 8 <= len {
            let size = u32::from_be_bytes([
                mp4[offset],
                mp4[offset + 1],
                mp4[offset + 2],
                mp4[offset + 3],
            ]) as usize;
            let typ = &mp4[offset + 4..offset + 8];
            if typ == b"mdat" && size != 0 {
                mp4[offset..offset + 4].copy_from_slice(&[0, 0, 0, 0]);
                return mp4;
            }
            if size == 0 {
                break;
            }
            offset += size.max(8);
        }
        mp4
    }

    /// 真实 fixture 优先（testtiny.mp4 带真实视频流），否则退回合成容器。
    fn media_body() -> (Vec<u8>, bool) {
        let candidates = [
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join("tiny.mp4"),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../testdata/tiny.mp4"),
        ];
        for p in candidates {
            if let Ok(raw) = std::fs::read(&p) {
                if !raw.is_empty() {
                    return (raw, true);
                }
            }
        }
        (synthetic_mp4(), false)
    }

    struct FailingClient;

    #[async_trait::async_trait]
    impl RoundTrip for FailingClient {
        async fn send(
            &self,
            _req: RawRequest,
            _timeout: Duration,
        ) -> std::result::Result<RawResponse, AppError> {
            Err(AppError::retryable(
                Code::NetworkError,
                Stage::Arguments,
                "connection refused (offline test)",
            ))
        }
    }

    /// 按 host 路由到脚本化响应的假传输层。
    struct FakeUpstream {
        state: StdMutex<UpstreamState>,
    }

    struct UpstreamState {
        yuanbao: Box<dyn Fn(&RawRequest) -> RawResponse + Send + Sync>,
        finder: Box<dyn Fn(&RawRequest) -> RawResponse + Send + Sync>,
        requests: Vec<RawRequest>,
    }

    fn json_resp(status: u16, body: String) -> RawResponse {
        RawResponse {
            status,
            content_length: Some(body.len() as i64),
            body: Box::pin(std::io::Cursor::new(body.into_bytes())),
        }
    }

    #[async_trait::async_trait]
    impl RoundTrip for FakeUpstream {
        async fn send(
            &self,
            req: RawRequest,
            _timeout: Duration,
        ) -> std::result::Result<RawResponse, AppError> {
            let mut s = self.state.lock().unwrap();
            s.requests.push(req.clone());
            let is_yuanbao = req.url.contains("yuanbao.tencent.com");
            let is_finder = req.url.contains("channels.weixin.qq.com");
            drop(s);
            let s = self.state.lock().unwrap();
            if is_yuanbao {
                return Ok((s.yuanbao)(&req));
            }
            if is_finder {
                return Ok((s.finder)(&req));
            }
            Ok(json_resp(404, r#"{"error":"no route"}"#.into()))
        }
    }

    /// 媒体假服务器：校验无 cookie、query 完整，返回媒体体。
    struct FakeMedia {
        body: Vec<u8>,
        requests: StdMutex<Vec<RawRequest>>,
        reject_cookie: bool,
    }

    #[async_trait::async_trait]
    impl RoundTrip for FakeMedia {
        async fn send(
            &self,
            req: RawRequest,
            _timeout: Duration,
        ) -> std::result::Result<RawResponse, AppError> {
            self.requests.lock().unwrap().push(req.clone());
            if self.reject_cookie && req.header("cookie").is_some() {
                return Ok(json_resp(500, "no cookie allowed".into()));
            }
            if !req.url.contains(SENTINEL_MEDIA) {
                return Ok(json_resp(404, "bad query".into()));
            }
            Ok(RawResponse {
                status: 200,
                content_length: Some(self.body.len() as i64),
                body: Box::pin(std::io::Cursor::new(self.body.clone())),
            })
        }
    }

    fn leak_env() -> (Deps, Arc<FakeUpstream>, Arc<FakeMedia>, PathBuf) {
        let (body, _) = media_body();
        let upstream = Arc::new(FakeUpstream {
            state: StdMutex::new(UpstreamState {
                yuanbao: Box::new(move |req| {
                    if req.header("cookie") != Some(SENTINEL_COOKIE) {
                        return json_resp(401, "{}".into());
                    }
                    json_resp(
                        200,
                        format!(
                            r#"{{"code":0,"data":{{"playable_url":"https://channels.weixin.qq.com/finder-preview/pages/feed?token={SENTINEL_TOKEN}&eid=EID1"}}}}"#
                        ),
                    )
                }),
                finder: Box::new(move |_| {
                    json_resp(
                        200,
                        format!(
                            r#"{{"errCode":0,"data":{{"feedInfo":{{"h264VideoInfo":{{"videoUrl":"https://cdn.example.com/v.mp4?sig={SENTINEL_MEDIA}"}},"description":"泄露测试 标题","picInfo":[]}},"authorInfo":{{"nickname":"作者"}}}}}}"#
                        ),
                    )
                }),
                requests: vec![],
            }),
        });
        let media = Arc::new(FakeMedia {
            body,
            requests: StdMutex::new(vec![]),
            reject_cookie: true,
        });
        let cfg_dir = tempdir().join("cfg");
        let cfg_dir_outer = cfg_dir.clone();
        let up2 = upstream.clone();
        let md2 = media.clone();
        let deps = Deps {
            config_dir: Box::new(move || cfg_dir.clone()),
            interactive: Box::new(|| false),
            upstream_factory: Box::new(move || {
                let mut c = Client::new(up2.clone());
                c.sleep = Some(Box::new(|_| {}));
                c
            }),
            media_factory: Box::new(move || md2.clone() as Arc<dyn RoundTrip>),
            cancel: CancelToken::default(),
        };
        (deps, upstream, media, cfg_dir_outer)
    }

    struct TestDeps {
        inner: Deps,
        cfg_dir: PathBuf,
    }

    fn new_test_deps() -> TestDeps {
        let cfg_dir = tempdir().join("cfg");
        let cfg2 = cfg_dir.clone();
        TestDeps {
            inner: Deps {
                config_dir: Box::new(move || cfg2.clone()),
                interactive: Box::new(|| false),
                upstream_factory: Box::new(|| {
                    let mut c = Client::new(Arc::new(FailingClient));
                    c.sleep = Some(Box::new(|_| {}));
                    c
                }),
                media_factory: Box::new(|| Arc::new(FailingClient)),
                cancel: CancelToken::default(),
            },
            cfg_dir,
        }
    }

    async fn run_cli(deps: &Deps, stdin_data: &str, argv: &[&str]) -> (i32, String, String) {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let stdin: Arc<Mutex<dyn Read + Send>> = Arc::new(Mutex::new(std::io::Cursor::new(
            stdin_data.as_bytes().to_vec(),
        )));
        let mut stdout = Vec::new();
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr: SharedWriter = stderr_vec.clone();
        let code = run(&argv, stdin, &mut stdout, stderr, deps).await;
        let err_bytes = stderr_vec.lock().unwrap().clone();
        (
            code,
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&err_bytes).into_owned(),
        )
    }

    fn install_creds(cfg_dir: &PathBuf) {
        let store = auth::Store::new(cfg_dir).unwrap();
        let lock = store.acquire_lock().unwrap();
        store
            .save(&auth::Credentials {
                saved_at: time::OffsetDateTime::now_utc(),
                source: auth::Source::ManualImport,
                verified_at: None,
                cookie: SENTINEL_COOKIE.into(),
                yuanbao_headers: [("x-id".to_string(), SENTINEL_HEADER.to_string())]
                    .into_iter()
                    .collect(),
            })
            .unwrap();
        lock.release().unwrap();
    }

    fn assert_no_sentinels(outputs: &[&str]) {
        let sentinels = [
            SENTINEL_COOKIE,
            SENTINEL_TOKEN,
            SENTINEL_MEDIA,
            SENTINEL_HEADER,
            "finder-preview/pages/feed?token",
        ];
        for out in outputs {
            for s in sentinels {
                assert!(!out.contains(s), "OUTPUT LEAK: sentinel {s} in:\n{out}");
            }
        }
    }

    #[tokio::test]
    async fn version_and_help() {
        let td = new_test_deps();
        let (code, out, _) = run_cli(&td.inner, "", &["version"]).await;
        assert_eq!(code, 0);
        assert!(out.starts_with("sph "), "version output: {out}");
        let (code, out, _) = run_cli(&td.inner, "", &["--help"]).await;
        assert_eq!(code, 0);
        assert!(out.contains("login"));
    }

    #[tokio::test]
    async fn inspect_requires_credentials() {
        let td = new_test_deps();
        let (code, out, err_out) =
            run_cli(&td.inner, "", &["inspect", "https://weixin.qq.com/sph/a"]).await;
        assert_eq!(code, 3);
        assert!(
            (err_out + &out).contains("sph login"),
            "message should point to sph login"
        );
    }

    #[tokio::test]
    async fn json_error_envelope_single_line() {
        let td = new_test_deps();
        let (code, out, _) = run_cli(
            &td.inner,
            "",
            &["inspect", "https://weixin.qq.com/sph/a", "--json"],
        )
        .await;
        assert_eq!(code, 3);
        let trimmed = out.trim_end_matches('\n');
        assert_eq!(
            trimmed.lines().count(),
            1,
            "stdout must hold exactly one line"
        );
        let env: serde_json::Value = serde_json::from_str(trimmed).unwrap();
        assert_eq!(env["ok"], false);
        assert_eq!(env["error"]["code"], "AUTH_REQUIRED");
    }

    #[tokio::test]
    async fn invalid_arguments_exit_code_2() {
        let td = new_test_deps();
        install_creds(&td.cfg_dir);
        let cases: Vec<Vec<&str>> = vec![
            vec!["inspect"],
            vec!["inspect", "u1", "u2"],
            vec!["inspect", "--stdin", "https://weixin.qq.com/sph/a"],
            vec!["download", "not-a-url"],
            vec!["download", "https://evil.test/sph/a"],
            vec!["download", "u://x", "--timeout", "0s"],
            vec!["download", "u://x", "--timeout", "nope"],
            vec!["download", "u://x", "--max-bytes", "-1"],
            vec!["download", "u://x", "--max-bytes", "1.5"],
            vec!["download", "u://x", "-o", "video.avi"],
            vec!["login", "https://weixin.qq.com/sph/a"],
            vec!["login", "--json"],
            vec!["auth", "import"],
        ];
        for argv in cases {
            let (code, _, _) = run_cli(&td.inner, "", &argv).await;
            assert_eq!(code, 2, "{argv:?}");
        }
    }

    #[tokio::test]
    async fn stdin_input_matrix() {
        let td = new_test_deps();
        // 空 stdin
        let (code, _, _) = run_cli(&td.inner, "", &["download", "--stdin"]).await;
        assert_eq!(code, 2);
        // 两个链接
        let (code, _, _) = run_cli(
            &td.inner,
            "https://weixin.qq.com/sph/a\nhttps://weixin.qq.com/sph/b",
            &["download", "--stdin"],
        )
        .await;
        assert_eq!(code, 2);
        // 带空白 trimming 的合法链接 → 越过参数校验进入凭证/网络阶段（退出码不是 2）
        install_creds(&td.cfg_dir);
        let (code, _, err_out) = run_cli(
            &td.inner,
            "  https://weixin.qq.com/sph/a  ",
            &["inspect", "--stdin"],
        )
        .await;
        assert_ne!(code, 2, "trimmed stdin should be accepted: {err_out}");
    }

    #[tokio::test]
    async fn auth_lifecycle() {
        let td = new_test_deps();
        // import
        let (code, out, _) = run_cli(
            &td.inner,
            "Cookie: session=xyz",
            &["auth", "import", "--stdin"],
        )
        .await;
        assert_eq!(code, 0);
        assert!(out.contains("已导入"));
        let store = auth::Store::new(td.cfg_dir.clone()).unwrap();
        let creds = store.load().unwrap();
        assert_eq!(creds.cookie, "session=xyz");
        assert_eq!(creds.verified_at, None);
        // status：显示来源与验证状态，永不显示值
        let (_, out, _) = run_cli(&td.inner, "", &["auth", "status"]).await;
        assert!(out.contains("manual_import"));
        assert!(out.contains("未验证"));
        assert!(!out.contains("session=xyz"), "status leaked cookie value");
        // logout
        let (code, out, _) = run_cli(&td.inner, "", &["logout"]).await;
        assert_eq!(code, 0);
        assert!(out.contains("已清除"));
        assert!(!store.exists());
        // 再次 logout 幂等
        let (code, _, _) = run_cli(&td.inner, "", &["logout"]).await;
        assert_eq!(code, 0);
        // auth clear 等价
        install_creds(&td.cfg_dir);
        let (code, _, _) = run_cli(&td.inner, "", &["auth", "clear"]).await;
        assert_eq!(code, 0);
        assert!(!store.exists());
    }

    #[tokio::test]
    async fn auth_import_rejects_garbage() {
        let td = new_test_deps();
        let (code, _, _) = run_cli(
            &td.inner,
            "curl 'https://x' -H 'cookie: a=b'",
            &["auth", "import", "--stdin"],
        )
        .await;
        assert_eq!(code, 2);
        let (code, _, _) = run_cli(&td.inner, "a=1\r\nb=2", &["auth", "import", "--stdin"]).await;
        assert_eq!(code, 2);
    }

    #[tokio::test]
    async fn login_requires_tty() {
        let td = new_test_deps();
        let (code, _, _) = run_cli(&td.inner, "", &["login"]).await;
        assert_eq!(code, 13, "non-TTY login must exit 13");
    }

    #[tokio::test]
    async fn non_login_commands_never_need_browser() {
        let td = new_test_deps();
        install_creds(&td.cfg_dir);
        let (code, _, _) = run_cli(&td.inner, "", &["auth", "status"]).await;
        assert_eq!(code, 0);
        let (code, _, _) = run_cli(&td.inner, "", &["logout"]).await;
        assert_eq!(code, 0);
        install_creds(&td.cfg_dir);
        let (code, _, _) = run_cli(
            &td.inner,
            "",
            &[
                "download",
                "https://weixin.qq.com/sph/a",
                "--timeout",
                "1ms",
            ],
        )
        .await;
        assert_ne!(
            code, 12,
            "download must not fail with LOGIN_DEPENDENCY_MISSING"
        );
    }

    #[test]
    fn timeout_flag_parsing() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1ms"), Some(Duration::from_millis(1)));
        assert_eq!(parse_duration("-1s"), Some(Duration::ZERO));
        assert_eq!(parse_duration("nope"), None);
        assert_eq!(parse_duration("0s"), Some(Duration::ZERO));
    }

    #[tokio::test]
    async fn inspect_e2e_isolation_no_leaks() {
        let (deps, _up, _md, cfg_dir) = leak_env();
        install_creds(&cfg_dir);
        let (code, out, err_out) = run_cli(
            &deps,
            "",
            &["inspect", "https://weixin.qq.com/sph/LeakTest1", "--json"],
        )
        .await;
        assert_eq!(code, 0, "stderr: {err_out}");
        assert_no_sentinels(&[&out, &err_out]);
        let env: serde_json::Value = serde_json::from_str(out.trim_end_matches('\n')).unwrap();
        assert_eq!(env["ok"], true);
        assert_eq!(env["data"]["title"], "泄露测试 标题");
        assert_eq!(env["data"]["local_id"].as_str().unwrap().len(), 12);
    }

    #[tokio::test]
    async fn download_e2e_isolation_no_leaks() {
        let (deps, _up, media, cfg_dir) = leak_env();
        install_creds(&cfg_dir);
        let work = tempdir();
        let out_path = work.join("out.mp4");
        let out_path_str = out_path.to_string_lossy().into_owned();
        let (body, real_video) = media_body();
        let argv: Vec<&str> = vec![
            "download",
            "https://weixin.qq.com/sph/LeakTest2",
            "-o",
            &out_path_str,
            "--json",
        ];
        let (code, out, err_out) = run_cli(&deps, "", &argv).await;
        assert_eq!(code, 0, "stderr: {err_out}");
        assert_no_sentinels(&[&out, &err_out]);
        let env: serde_json::Value = serde_json::from_str(out.trim_end_matches('\n')).unwrap();
        assert_eq!(env["ok"], true);
        assert_eq!(env["data"]["bytes"], body.len() as i64);
        assert!(!env["data"]["sha256"].as_str().unwrap().is_empty());
        let has_ffprobe = crate::verify::has_ffprobe();
        let verification = env["data"]["verification"].as_str().unwrap();
        if has_ffprobe && real_video {
            assert_eq!(
                verification, "ffprobe",
                "expected full ffprobe verification"
            );
        } else {
            assert_eq!(verification, "container");
        }
        let stored = std::fs::metadata(&out_path).unwrap();
        assert_eq!(stored.len(), body.len() as u64);
        // 媒体请求不得携带 cookie
        {
            let media_reqs = media.requests.lock().unwrap();
            for r in media_reqs.iter() {
                assert!(
                    r.header("cookie").is_none(),
                    "media request carried a cookie"
                );
            }
        }

        // 重新下载不加 --overwrite → FILE_EXISTS (9)
        let argv2: Vec<&str> = vec![
            "download",
            "https://weixin.qq.com/sph/LeakTest2",
            "-o",
            &out_path_str,
        ];
        let (code2, out2, err2) = run_cli(&deps, "", &argv2).await;
        assert_eq!(
            code2, 9,
            "re-download exit {code2}, want 9 (FILE_EXISTS); stderr: {err2}"
        );
        assert_no_sentinels(&[&out2, &err2]);
        let still = std::fs::metadata(&out_path).unwrap();
        assert_eq!(
            still.len(),
            body.len() as u64,
            "existing file damaged by failed re-download"
        );
    }

    #[tokio::test]
    async fn download_streams_large_body() {
        let (_deps, upstream, _media, cfg_dir) = leak_env();
        install_creds(&cfg_dir);
        // 换成 ~8 MiB 的媒体体
        let head = patch_mdat_to_eof(media_body().0);
        let chunk = vec![7u8; 256 << 10];
        let mut big = head.clone();
        for _ in 0..32 {
            big.extend_from_slice(&chunk);
        }
        let big_media = Arc::new(FakeMedia {
            body: big.clone(),
            requests: Mutex::new(vec![]),
            reject_cookie: true,
        });
        let cfg2 = cfg_dir.clone();
        let up2 = upstream.clone();
        let deps2 = Deps {
            config_dir: Box::new(move || cfg2.clone()),
            interactive: Box::new(|| false),
            upstream_factory: {
                let up = up2.clone();
                Box::new(move || {
                    let mut c = Client::new(up.clone());
                    c.sleep = Some(Box::new(|_| {}));
                    c
                })
            },
            media_factory: Box::new({
                let m = big_media.clone();
                move || m.clone() as Arc<dyn RoundTrip>
            }),
            cancel: CancelToken::default(),
        };
        let work = tempdir();
        let out_path = work.join("big.mp4");
        let out_path_str = out_path.to_string_lossy().into_owned();
        let argv: Vec<&str> = vec![
            "download",
            "https://weixin.qq.com/sph/LeakBig1",
            "-o",
            &out_path_str,
        ];
        let (code, _, err_out) = run_cli(&deps2, "", &argv).await;
        assert_eq!(code, 0, "big download stderr: {err_out}");
        let stored = std::fs::metadata(&out_path).unwrap();
        assert_eq!(stored.len(), big.len() as u64);
    }
}

#[cfg(test)]
mod m2_tests {
    use super::*;
    use crate::session::{accounts_root, AccountDir, DEFAULT_ACCOUNT};
    use std::path::Path;

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("sph-cli-m2-{tag}-{:016x}", {
            use rand::Rng;
            rand::thread_rng().gen::<u64>()
        }));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn m2_deps(cfg_dir: PathBuf) -> Deps {
        Deps {
            config_dir: Box::new(move || cfg_dir.clone()),
            interactive: Box::new(|| false),
            upstream_factory: Box::new(|| {
                Client::new(Arc::new(crate::http::ReqwestTransport::api()))
            }),
            media_factory: Box::new(|| Arc::new(crate::http::ReqwestTransport::media())),
            cancel: CancelToken::default(),
        }
    }

    async fn run_cli2(deps: &Deps, stdin_data: &str, argv: &[&str]) -> (i32, String, String) {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let stdin: Arc<Mutex<dyn Read + Send>> = Arc::new(Mutex::new(std::io::Cursor::new(
            stdin_data.as_bytes().to_vec(),
        )));
        let mut stdout = Vec::new();
        let stderr_vec: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr: SharedWriter = stderr_vec.clone();
        let code = run(&argv, stdin, &mut stdout, stderr, deps).await;
        let err_out = stderr_vec.lock().unwrap().clone();
        (
            code,
            String::from_utf8_lossy(&stdout).into_owned(),
            String::from_utf8_lossy(&err_out).into_owned(),
        )
    }

    fn test_video(dir: &Path) -> PathBuf {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/tiny.mp4");
        let dst = dir.join("v.mp4");
        std::fs::copy(&src, &dst).unwrap();
        dst
    }

    #[tokio::test]
    async fn publish_argument_errors() {
        let cfg = tempdir("pubargs").join("cfg");
        let deps = m2_deps(cfg);
        let work = tempdir("work");
        let video = test_video(&work);
        let video_str = video.to_string_lossy().into_owned();

        // 缺视频参数
        let (code, _, _) = run_cli2(&deps, "", &["publish", "--title", "t"]).await;
        assert_eq!(code, 2);
        // 空标题
        let (code, _, _) = run_cli2(&deps, "", &["publish", &video_str]).await;
        assert_eq!(code, 2);
        // 视频不存在
        let (code, _, _) = run_cli2(&deps, "", &["publish", "/nope/x.mp4", "--title", "t"]).await;
        assert_eq!(code, 2);
        // 封面不存在
        let (code, _, _) = run_cli2(
            &deps,
            "",
            &[
                "publish",
                &video_str,
                "--title",
                "t",
                "--cover",
                "/nope/c.jpg",
            ],
        )
        .await;
        assert_eq!(code, 2);
        // 多位置参数
        let (code, _, _) = run_cli2(
            &deps,
            "",
            &["publish", &video_str, "extra.mp4", "--title", "t"],
        )
        .await;
        assert_eq!(code, 2);
    }

    #[tokio::test]
    async fn publish_without_session_is_15() {
        let cfg = tempdir("pubnosess").join("cfg");
        let deps = m2_deps(cfg);
        let work = tempdir("work");
        let video = test_video(&work);
        let video_str = video.to_string_lossy().into_owned();
        let (code, out, _) = run_cli2(
            &deps,
            "",
            &["publish", &video_str, "--title", "t", "--json"],
        )
        .await;
        assert_eq!(code, 15, "want SESSION_EXPIRED(15)");
        let env: serde_json::Value = serde_json::from_str(out.trim_end_matches('\n')).unwrap();
        assert_eq!(env["error"]["code"], "SESSION_EXPIRED");
        assert_eq!(env["error"]["stage"], "session_load");
    }

    #[tokio::test]
    async fn accounts_lists_both_domains() {
        let cfg = tempdir("accts").join("cfg");
        std::fs::create_dir_all(
            AccountDir::new(&accounts_root(&cfg), DEFAULT_ACCOUNT).profile_dir(),
        )
        .unwrap();
        let deps = m2_deps(cfg);
        let (code, out, _) = run_cli2(&deps, "", &["accounts"]).await;
        assert_eq!(code, 0);
        assert!(out.contains("助手会话"), "out: {out}");
        assert!(out.contains("default"), "out: {out}");
        assert!(out.contains("下载凭证"), "out: {out}");
        assert!(!out.contains("session="), "不得泄漏凭证值");
    }

    #[tokio::test]
    async fn logout_assistant_removes_session_only() {
        let cfg = tempdir("lo").join("cfg");
        let dir = AccountDir::new(&accounts_root(&cfg), DEFAULT_ACCOUNT);
        std::fs::create_dir_all(dir.profile_dir()).unwrap();
        std::fs::write(dir.profile_dir().join("Cookies"), b"x").unwrap();
        let deps = m2_deps(cfg.clone());
        let (code, out, _) = run_cli2(&deps, "", &["logout", "--assistant"]).await;
        assert_eq!(code, 0);
        assert!(out.contains("助手会话"), "out: {out}");
        assert!(!dir.root.exists(), "profile must be removed");
        // 幂等
        let (code, _, _) = run_cli2(&deps, "", &["logout", "--assistant"]).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn parse_tags_splitting() {
        assert_eq!(
            parse_tags(Some("机械,科普，儿童")),
            vec!["机械", "科普", "儿童"]
        );
        assert_eq!(parse_tags(Some("  a , ,b,,")), vec!["a", "b"]);
        assert_eq!(parse_tags(None), Vec::<String>::new());
        assert_eq!(parse_tags(Some("")), Vec::<String>::new());
    }
}
