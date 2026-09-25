//! 输出契约：--json 模式下 stdout 恰好一个 JSON 对象；其余诊断全走 stderr。

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};

use crate::apperr::{AppError, Code, Result, Stage};

/// --json 模式下打印的单个 JSON 信封。
pub fn success_envelope(command: &str, data: Value) -> Value {
    json!({"ok": true, "command": command, "data": data})
}

pub fn error_envelope(err: &AppError) -> Value {
    json!({
        "ok": false,
        "error": {
            "code": err.code.as_str(),
            "stage": err.stage.as_str(),
            "message": err.message,
            "retryable": err.retryable,
        }
    })
}

/// 恰好在 stdout 打印一个 JSON 对象加换行。
pub fn write_json(stdout: &mut dyn Write, env: &Value) -> Result<()> {
    let mut raw = serde_json::to_string(env)
        .map_err(|_| AppError::new(Code::InternalError, Stage::Arguments, "结果序列化失败"))?;
    raw.push('\n');
    stdout
        .write_all(raw.as_bytes())
        .map_err(|_| AppError::new(Code::IOError, Stage::Arguments, "无法写出结果"))?;
    Ok(())
}

/// 报告错误：JSON 模式下一个 JSON 对象，否则错误流上一行受控文本。返回映射后的退出码。
pub fn fail(
    err: &AppError,
    json_mode: bool,
    stdout: &mut dyn Write,
    err_out: &mut dyn Write,
) -> i32 {
    if json_mode {
        let _ = write_json(stdout, &error_envelope(err));
    } else {
        let _ = writeln!(
            err_out,
            "错误 [{}/{}]: {}",
            err.code.as_str(),
            err.stage.as_str(),
            err.message
        );
    }
    err.code.exit_code()
}

/// stderr 进度回调，节流到每秒一行。
pub struct ProgressPrinter {
    last: Option<Instant>,
    out: Arc<Mutex<dyn Write + Send>>,
}

impl ProgressPrinter {
    pub fn new(out: Arc<Mutex<dyn Write + Send>>) -> ProgressPrinter {
        ProgressPrinter { last: None, out }
    }

    pub fn call(&mut self, sent: i64, total: i64) {
        let now = Instant::now();
        if let Some(last) = self.last {
            if now.duration_since(last) < std::time::Duration::from_secs(1) {
                return;
            }
        }
        self.last = Some(now);
        let line = if total > 0 {
            format!(
                "\r下载中 {} / {} ({:.1}%)",
                human_bytes(sent),
                human_bytes(total),
                sent as f64 / total as f64 * 100.0
            )
        } else {
            format!("\r下载中 {}", human_bytes(sent))
        };
        let _ = self.out.lock().map(|mut w| w.write_all(line.as_bytes()));
    }
}

pub fn human_bytes(n: i64) -> String {
    const UNIT: i64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    let mut div = UNIT;
    let mut exp = 0;
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    format!(
        "{:.1} {}iB",
        n as f64 / div as f64,
        "KMGTPE".chars().nth(exp).unwrap_or('P')
    )
}

pub fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

pub const HELP_TEXT: &str = "sph — 微信视频号本地自动化工具（v2）

用法:
  sph login [--timeout 5m] [--account NAME]   扫码登录视频号助手（持久会话）
  sph login --yuanbao                         登录元宝（下载解析凭证，一次性浏览器）
  sph publish VIDEO.mp4 --title \"标题\" [选项]  发布视频到视频号
  sph batch <目录> [选项]                      目录内 .mp4 顺序上架（标题=文件名，同名图片自动封面）
  sph accounts                                查看本地账号会话状态（不联网）
  sph history [--limit N] [--json]            查看发布恢复轨迹
  sph doctor [--json]                         健康检查（会话/凭证/补丁/浏览器）
  sph patch export [--output F]               导出 selector 补丁
  sph patch import <file>                     导入补丁（未知字段报错）
  sph inspect URL [--json] [--timeout 60s]    解析分享链接，查看视频信息
  sph download URL [-o FILE.mp4] [--overwrite] [--max-bytes N] [--json]
  pbpaste | sph download --stdin              从 stdin 读取链接
  sph auth status                             查看下载凭证状态（不联网）
  sph logout [--assistant]                    清除下载凭证 / 助手会话
  sph auth import --stdin [--headers-file F]  故障备用：手动导入 Cookie
  sph version                                 显示版本

publish 选项:
  --title \"标题\"        必填
  --description \"描述\"
  --tags \"机械,科普\"     逗号分隔
  --cover FILE.jpg       封面图
  --at \"YYYY-MM-DD HH:MM\"  定时发表（本地时区，默认立即）
  --collection \"名称\"    加入合集
  --link \"名称\"          添加链接
  --activity \"名称\"      关联活动
  --ai-mark              标注含 AI 生成内容
  --account NAME         使用指定账号（默认 default）
  --dry-run              走完除提交外的全部步骤
  --headed               可见浏览器（调试用）
  --json                 单 JSON 对象输出
  --timeout <时长>       覆盖默认超时

batch 选项: --tags / --description / --account / --dry-run / --headed / --json / --timeout

说明:
  - 首次使用执行 sph login：扫码后登录态保存在本机专用 profile，后续
    publish 直接复用，不需要重复扫码。
  - 平时的 inspect / download / publish 均为非交互，适合脚本与 Agent 调用。
  - 输出文件默认不覆盖同名文件；需要覆盖时加 --overwrite。
  - JSON 模式下 stdout 只输出一个 JSON 对象。
";
