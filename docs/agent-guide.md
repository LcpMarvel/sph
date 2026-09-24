# sph Agent 集成指南

本文件面向**能执行 Shell 命令的 AI Agent**（Claude Code、Codex、WorkBuddy 等），描述如何安装并调用 sph。人类用户手册见 [README](../README.md)。

sph 是本地视频号自动化 CLI：发布、定时发表、批量上架、下载。单二进制、零运行时依赖、local-first。**不要把本文件喂给模型之外的读者**——它就是给你看的。

## 1. 安装

从 Release 下载（按平台选文件）或源码安装：

```bash
curl -fsSL -o /tmp/sph.zip \
  https://github.com/LcpMarvel/sph/releases/latest/download/sph-darwin-arm64.zip
unzip -o -j /tmp/sph.zip -d /usr/local/bin && chmod +x /usr/local/bin/sph-darwin-arm64
mv /usr/local/bin/sph-darwin-arm64 /usr/local/bin/sph   # 产物名带平台后缀，重命名顺手

# 或有 Rust 工具链：
cargo install --git https://github.com/LcpMarvel/sph
```

首次使用需要用户扫码（唯一的人工步骤）：

```bash
sph login        # 用户亲自扫码；会话持久保存，之后不再扫
```

## 2. 调用契约

除 `login` 外全部命令**非交互**。三条铁律：

1. **`--json` 模式下 stdout 恰好一个 JSON 对象**（进度/诊断全在 stderr）——直接 `json.loads(stdout)`。
2. **退出码稳定可编程**（见 §3），按码决策，不要解析错误文本。
3. **错误信息可以原样转述给用户**——已消毒，不含凭证/签名 URL。

```bash
sph publish ./video.mp4 --title "..." --tags "机械,科普" --json
# 成功: {"ok":true,"command":"publish","data":{"account":"default","video":"...","title":"...","dry_run":false,"submitted":true}}
# 失败: {"ok":false,"error":{"code":"SESSION_EXPIRED","stage":"navigate","message":"...","retryable":false}}
```

**Agent 不要自动化 `sph login`**（需要用户扫码）。遇到 3/15 退出码时，把命令原样提示给用户执行。

## 3. 退出码表

| 退出码 | 含义 | Agent 应对 |
| --- | --- | --- |
| 0 | 成功 | 继续 |
| 2 | 参数错误 | 检查参数，不要重试 |
| 3 | 下载凭证缺失/失效 | 提示用户 `sph login --yuanbao` |
| 4 / 5 | 网络 / 上游失败 | 可重试（有退避） |
| 9 | 文件已存在 | 换输出路径或加 `--overwrite` |
| 15 | 助手会话失效 | 提示用户 `sph login` 重新扫码 |
| 16 | 平台拒绝（审核/校验） | **不要重试**，原样转述给用户 |
| 17 | 自动恢复后仍失败 | 提示用户看 `~/.sph/crashes/` 现场 |
| 18 | 定时参数非法 | 修正 `--at` 时间 |
| 130 | 用户取消 | 停止 |

## 4. 常用任务

```bash
# 发一条视频
sph publish ./v.mp4 --title "标题" --description "描述" --tags "机械,科普" --cover c.jpg --json

# 定时发表（本地时区，必须未来时间）
sph publish ./v.mp4 --title "标题" --at "2026-09-25 20:00" --json

# 扩展属性：合集 / 链接 / 活动 / AI 标注
sph publish ./v.mp4 --title "标题" --collection "机械系列" --ai-mark --json

# 批量上架一个目录（标题=文件名，同名图片自动封面；逐条独立结果）
sph batch ./videos/ --json

# 下载视频
sph download "https://weixin.qq.com/sph/xxxx" -o out.mp4 --json

# 先验证不写库（强烈建议首次接入时先跑一遍）
sph publish ./v.mp4 --title "测试" --dry-run --json

# 排查
sph doctor --json          # 环境健康检查（会话/凭证/补丁/浏览器）
sph history --json         # 发布与自动恢复轨迹
```

## 5. 安全边界（Agent 必须遵守）

- **绝不读取、展示、传输** `~/.sph/` 下的任何文件内容（凭证、profile、轨迹可能含敏感信息）；只通过 `sph` 命令与之交互。
- 发布是**真实对外动作**：未经用户明确确认不要带 `--at` 定时或去掉 `--dry-run` 实发。
- `sph logout --assistant` / `sph logout` 会清除会话——执行前需用户确认。
- 错误消息可以转述；现场目录（`~/.sph/crashes/`）里的 `page.txt`/`screenshot.png` 可以提供给用户或更高层模型分析，但不要外发。

## 6. 故障速查

| 现象 | 处置 |
| --- | --- |
| 退出 15 | 用户重新 `sph login` 扫码 |
| 退出 17 | `sph history` 看恢复轨迹；`~/.sph/crashes/` 有页面快照+截图 |
| 页面改版导致流程卡住（退出 11/17） | 写 `~/.sph/patches/publish.json` 覆盖失效 selector，`sph patch import` 亦可；改完重试 |
| 定时没生效 | 平台对定时范围有限制；重试用更晚的时间 |
