# sph — 微信视频号本地自动化工具

[![CI](https://github.com/LcpMarvel/sph/actions/workflows/ci.yml/badge.svg)](https://github.com/LcpMarvel/sph/actions/workflows/ci.yml)

单二进制命令行工具：扫码登录一次视频号助手，之后在本机完成**发布、定时发表、批量上架、下载**——所有命令非交互、`--json` 输出、退出码稳定可编程，人和 AI Agent 用同一套接口。

```bash
sph login                                  # 扫码登录（持久会话，只此一次）
sph publish ./video.mp4 --title "齿轮是怎么工作的" --tags "机械,科普"
sph publish ./video.mp4 --title "..." --at "2026-09-25 20:00"   # 定时发表
sph batch ./videos/                        # 整目录顺序上架（标题取文件名）
sph download "https://weixin.qq.com/sph/xxxx" -o video.mp4      # 下载
```

- 登录态保存在本机专用浏览器 profile，**后续任何命令不再扫码**。
- 平时的 publish / download 全部 headless，不弹窗口。
- 只自动化你本人有权操作的账号；所有操作走真实页面，与人工同路径。

## 安装

从 [Releases](https://github.com/LcpMarvel/sph/releases) 下载对应平台压缩包，解压即用：

| 文件 | 平台 |
| --- | --- |
| `sph-darwin-arm64.zip` | macOS Apple Silicon |
| `sph-darwin-amd64.zip` | macOS Intel |
| `sph-linux-amd64.tar.gz` | Linux x86_64 |
| `sph-linux-arm64.tar.gz` | Linux ARM64 |
| `sph-windows-amd64.zip` | Windows x64 |
| `sph-windows-arm64.zip` | Windows ARM64 |

有 Rust 工具链也可以源码安装：

```bash
cargo install --git https://github.com/LcpMarvel/sph
```

运行时零外部依赖（`ffprobe` 可选，装了会做完整视频流校验）。优先使用系统 Chrome/Edge；没有时首次运行自动下载专用 Chromium（约 200MB，npmmirror/Google 双源竞速，国内自动走镜像）。

## 命令一览

```bash
# 账号（两个凭证域，互不混用）
sph login [--account NAME]              # 视频号助手扫码（持久 profile 会话）
sph login --yuanbao                     # 元宝登录（下载解析凭证，一次性浏览器）
sph accounts                            # 双域状态一览
sph logout [--assistant]                # 清下载凭证 / 助手会话

# 发布
sph publish VIDEO.mp4 --title "标题" [选项]
    --description "描述"  --tags "机械,科普"  --cover cover.jpg
    --at "YYYY-MM-DD HH:MM"               # 定时发表（本地时区）
    --collection "名称"   --link "名称"   --activity "名称"
    --ai-mark                             # 视频标注：含 AI 生成内容
    --account NAME                        # 多账号
    --dry-run                             # 走完除提交外全部步骤
    --headed                              # 可见浏览器（调试）
    --json                                # 单 JSON 对象输出

# 批量
sph batch <dir> [--tags "..."] [--dry-run] [--json]
    # 目录内 .mp4 顺序发布；标题=文件名；同名 .jpg/.jpeg/.png 自动作封面

# 下载（v1 能力原样保留）
sph download "https://weixin.qq.com/sph/xxxx" [-o out.mp4] [--overwrite] [--json]
sph inspect  "https://weixin.qq.com/sph/xxxx" [--json]
pbpaste | sph download --stdin

# 运维
sph history [--limit N] [--json]        # 发布/恢复轨迹
sph doctor [--json]                     # 健康检查（会话/凭证/补丁/浏览器）
sph patch export [--output F]           # 导出 selector 补丁
sph patch import <file>                 # 导入补丁（校验字段名）
sph version / --help
```

## 给脚本 / AI Agent 用

**任意 Agent（一条命令）**：通过 [skills CLI](https://github.com/vercel-labs/skills)，自动识别本机已装的 Agent（Claude Code、Codex、Cursor 等 90+）并装到各自目录：

```bash
npx skills add LcpMarvel/sph
```

**Claude Code** 也可以装插件（带命名空间与版本管理）：

```
/plugin marketplace add LcpMarvel/sph
/plugin install sph@sph
```

**手动**：把 `skills/sph/` 拷到对应 skills 目录（Codex 用 `~/.codex/skills/`，多 Agent 共享用 `~/.agents/skills/`）。不支持 skill 的 Agent 按 [docs/agent-guide.md](docs/agent-guide.md) 接入。调用契约：除 `login`（需扫码）外所有命令**非交互**。`--json` 模式下 stdout 恰好一个 JSON 对象，进度与诊断全在 stderr；退出码稳定可编程：

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 2 | 参数错误 |
| 3 | 凭证缺失/失效（重新 login） |
| 4 / 5 | 网络 / 上游失败 |
| 9 | 文件已存在 |
| 15 | 助手会话失效（`sph login` 重新扫码） |
| 16 | 平台拒绝（审核/校验未过） |
| 17 | 自动恢复后仍失败（现场已保存） |
| 18 | 定时参数非法 |
| 130 | 用户取消 |

```bash
sph publish ./v.mp4 --title "..." --json
# {"ok":true,"command":"publish","data":{"account":"default","video":"...","title":"...","dry_run":false,"submitted":true}}

# 失败时：
# {"ok":false,"error":{"code":"SESSION_EXPIRED","stage":"navigate","message":"...","retryable":false}}
```

## 自愈与补丁

发布流程是固定 selector 的确定性流水线，内置 **RuleBackend 自愈**：遇到阻塞对话框（"我知道了/确定/切换"等）自动点掉、页面未加载完自动等待重试；恢复全程留痕（`sph history`），失败现场（页面快照 + 截图）保存在 `~/.sph/crashes/`。

页面改版导致 selector 失效时，**不用等发版**——写补丁即可：

```json
// ~/.sph/patches/publish.json
{ "selectors": { "title_input": "input[placeholder*='新标题']" } }
```

`sph patch import/export` 可分享补丁；坏补丁（JSON 损坏、字段名拼错）会被响亮拒绝。

## 数据与安全

- 一切状态在 `~/.sph/`（`$SPH_CONFIG_DIR` 可覆盖）：账号 profile、补丁、轨迹、崩溃现场。local-first，无服务端。
- 助手会话 = 持久 Chromium profile 本身，**不采集/存储任何 cookie**；下载凭证单独存放（0600、原子写入、flock 保护）。
- 输出（含 JSON、错误、轨迹）不含 cookie/token/签名 URL；错误信息经消毒，可原样转述。
- 声明类选项默认保守（不勾原创）；平台默认勾选原创时会响亮报错交人工确认。
- 下载走元宝解析链 + 直连下载（快速稳定），与助手会话两个凭证域独立。

## 卸载

```bash
sph logout --assistant && sph logout
rm -rf ~/.sph
```

## 开发

```bash
make build    # cargo build --release
make test     # cargo test（含真实 Chromium fixture e2e）
make check    # fmt + clippy
```

发布新版本：推送 `v*` 标签触发六平台 Release：

```bash
git tag v2.0.0 && git push origin v2.0.0
```

结构：`src/session`（账号会话）、`src/publish`（发布流水线 + 恢复层 + 补丁）、`src/download` / `src/upstream`（下载链）、`src/browser`（CDP 运行时 + 页面快照）、`src/cli`（命令面与输出契约）。浏览器自动化基于 [chromiumoxide](https://github.com/mattsse/chromiumoxide)（CDP 直连，tokio）。
