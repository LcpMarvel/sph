---
name: sph
description: 通过 sph CLI 操作微信视频号——发布/定时发布视频、查看账号与发布历史、解析并下载视频号分享链接、登录态与凭证排查。当用户提到视频号、sph、把视频发到视频号、下载视频号视频、WeChat Channels 时使用。
---

# sph 视频号自动化

sph 是用户本机的单二进制 CLI：发布、定时发表、下载、账号管理。你通过 Bash 调用它，**不要自己实现任何视频号 API/页面调用**。

完整命令与选项见 [reference/commands.md](reference/commands.md)。

## 三条铁律

1. **所有命令带 `--json`**：stdout 恰好一个 JSON 对象，直接解析；进度和诊断在 stderr。
2. **按退出码决策**（见下表），不要靠解析错误文本分类；但 JSON 里的 `error.message` 已消毒，可原样转述给用户。
3. **绝不替用户跑 `sph login`**——需要本人扫码。遇到退出码 3 / 15，把命令原样交给用户执行（在 Claude Code 里提示用户输入 `! sph login`，输出会回到会话中）。

## 前置检查

第一次接到 sph 任务时：

```bash
command -v sph && sph doctor --json
```

`sph doctor` 一次给出会话/凭证/补丁/浏览器健康状态。

- **未安装**：按用户平台从 Releases 下载（macOS ARM 示例：`curl -fsSL -o /tmp/sph.zip https://github.com/LcpMarvel/sph/releases/latest/download/sph-darwin-arm64.zip && unzip -o -j /tmp/sph.zip -d /usr/local/bin && mv /usr/local/bin/sph-darwin-arm64 /usr/local/bin/sph`），或有 Rust 工具链时 `cargo install --git https://github.com/LcpMarvel/sph`。装完先确认 `sph version`。
- **未登录**：提示用户自己执行 `sph login` 扫码（会话持久，只此一次）。下载功能另需 `sph login --yuanbao`。两个凭证域状态随时可用 `sph accounts` 查看。

## 核心工作流

### 发布视频（真实对外动作，必须先确认）

1. 向用户复述将使用的参数（视频文件、标题、描述、标签、封面、账号、是否定时），**等用户确认**。
2. 首次接入或距上次成功发布较久时，先 `--dry-run` 验证一遍。
3. 实发：

```bash
sph publish ./video.mp4 --title "标题" --description "描述" --tags "机械,科普" --cover cover.jpg --json
```

- 定时发表加 `--at "YYYY-MM-DD HH:MM"`（本地时区、必须未来时间）；定时属于延迟生效的对外动作，同样要先确认。
- 扩展属性：`--collection "合集名"` / `--link "链接名"` / `--activity "活动名"` / `--ai-mark`（含 AI 生成内容标注）。
- 多账号加 `--account NAME`（账号列表用 `sph accounts` 查）。

### 批量上架

```bash
sph batch ./videos/ --json
```

- 目录内 `.mp4` 顺序发布，标题=文件名，同名 `.jpg/.jpeg/.png` 自动作封面；逐条独立结果，退出码=首个失败码。
- 批量是对外动作的放大器：**先向用户确认目录内容（列出将发布的文件清单），再 `--dry-run` 一遍，最后才实发**。

### 下载视频号视频

```bash
sph download "https://weixin.qq.com/sph/xxxx" -o out.mp4 --json
```

- 需要元宝凭证；退出码 3 → 提示用户 `sph login --yuanbao`。
- 退出码 9（文件已存在）→ 换路径或加 `--overwrite`（覆盖前确认）。
- 只解析不下载用 `sph inspect "<url>" --json`。

### 排查

- `sph doctor --json`：环境健康检查（会话/凭证/补丁/浏览器），排查第一步。
- `sph accounts` / `sph auth status`：两个凭证域的状态（不联网）。
- `sph history --json`：发布与自动恢复轨迹。
- 退出码 11（页面改版 selector 失效）：可用补丁覆盖——`sph patch export` 导出当前 selector 到文件，改失效字段后 `sph patch import <file>`（未知字段会响亮报错），改完重试。
- 退出码 17：现场已存到 `~/.sph/crashes/`——里面的 `page.txt`/`screenshot.png` 可以读给用户或用于本地分析，**不要外发**。

## 退出码 → 动作

| 码 | 含义 | 你的动作 |
| --- | --- | --- |
| 0 | 成功 | 继续 |
| 2 | 参数错误 | 检查参数，不要原样重试 |
| 3 | 下载凭证缺失/失效 | 提示用户 `sph login --yuanbao` |
| 4 / 5 | 网络 / 上游失败（含限流） | 可退避重试 |
| 6 / 7 / 8 | 视频不可用 / 媒体不支持 / 下载失败或超限 | 转述给用户，按 message 调整 |
| 9 | 文件已存在 | 换输出路径或确认后 `--overwrite` |
| 11 | 页面改版（selector 失效） | 转述给用户；可 `sph patch export` 改 selector 后 `patch import` 重试 |
| 12 / 14 | 登录依赖缺失 / 登录失败或占用 | 转述，必要时让用户重跑 login |
| 15 | 助手会话失效 | 提示用户重新 `sph login` 扫码 |
| 16 | 平台拒绝（审核/校验未过） | **不要重试**，原样转述 |
| 17 | 自动恢复后仍失败 | 看 `sph history` 与 `~/.sph/crashes/` 现场 |
| 18 | 定时参数非法 | 修正 `--at`（须未来时间） |
| 130 | 用户取消 | 停止，不要重试 |

## 安全边界（必须遵守）

- **绝不读取、展示、传输 `~/.sph/` 下的文件内容**（凭证、浏览器 profile、轨迹均敏感）；只能通过 `sph` 命令交互。唯一例外是排查退出码 17 时的 crashes 现场，且仅限本地。
- 发布、定时发表是真实对外动作：未经用户明确确认，不得去掉 `--dry-run` 实发或加 `--at` 定时。
- `sph logout` / `sph logout --assistant` 会清除凭证/会话，执行前必须用户确认。
