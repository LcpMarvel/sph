# sph Agent 集成指南

本文件面向**能执行 Shell 命令的 AI Agent**（WorkBuddy、Claude Code、Codex 等），描述如何为用户安装并调用 sph。人类用户手册见 [README](../README.md)。

sph 是一个本地 CLI：把微信视频号分享链接（`https://weixin.qq.com/sph/xxxx`）解析并下载为 MP4。纯单二进制，无运行时依赖。

## 1. 安装

从 GitHub Release 下载对应平台的压缩包，解压即用（无需任何依赖）：

```bash
# 判断平台后选择对应文件（os = darwin / linux / windows；arch = amd64 / arm64）
# macOS arm64 示例：
curl -fsSL -o /tmp/sph.zip \
  https://github.com/LcpMarvel/sph-downloader/releases/latest/download/sph-darwin-arm64.zip
unzip -o -j /tmp/sph.zip -d /usr/local/bin && chmod +x /usr/local/bin/sph-darwin-arm64
```

| 平台 | 文件 |
| --- | --- |
| macOS Apple Silicon | `sph-darwin-arm64.zip` |
| macOS Intel | `sph-darwin-amd64.zip` |
| Linux x86_64 | `sph-linux-amd64.tar.gz` |
| Linux ARM64 | `sph-linux-arm64.tar.gz` |
| Windows x64 | `sph-windows-amd64.zip`（内含 .exe） |
| Windows ARM64 | `sph-windows-arm64.zip`（内含 .exe） |

校验（可选）：下载同目录的 `checksums.txt` 比对 SHA-256。

也可以 `git clone` 后 `go build -o sph ./cmd/sph`（需要 Go ≥ 1.23）。

## 2. 前提：登录凭证（唯一需要人类的步骤）

`download` / `inspect` 需要用户本人的元宝登录凭证：

1. 先运行 `sph auth status`（本地检查，不联网）确认凭证状态；
2. 无凭证（退出码 3 / `AUTH_REQUIRED`）时，**引导用户在自己的终端运行**：

   ```
   sph login
   ```

   会弹出一个专用浏览器窗口，用户用微信扫码即完成；程序自动检测登录、保存凭证并关闭浏览器。首次运行会自动下载专用浏览器（约 200MB，国内网络自动走镜像），请提示用户耐心等待。
3. 凭证失效（退出码 3 / `INVALID_CREDENTIALS`）时同样引导用户重新 `sph login`。

## 3. 调用契约

- 除 `login` 外所有命令均**非交互**；加 `--json` 后 **stdout 恰好一个 JSON 对象**（进度与诊断在 stderr），可直接整体解析；
- 成功：`{"ok":true,"command":"...","data":{...}}`；失败：`{"ok":false,"error":{"code":"...","stage":"...","message":"...","retryable":bool}}`；
- `message` 永远是安全的中文说明（不含 Cookie、token、签名 URL），可直接转述给用户。

常用命令：

```bash
# 解析（不下载）
sph inspect "https://weixin.qq.com/sph/xxxx" --json
# → {"ok":true,"command":"inspect","data":{"local_id":"...","title":"...","author":"...","media_source":"h264VideoInfo","codec_hint":"h264"}}

# 下载
sph download "https://weixin.qq.com/sph/xxxx" -o /path/to/out.mp4 --json
# → {"ok":true,"command":"download","data":{"path":"...","bytes":123,"sha256":"...","verification":"ffprobe"}}
```

`download` 规则：

- `-o` 路径必须以 `.mp4` 结尾、父目录必须已存在；
- 目标已存在时默认失败（`FILE_EXISTS` / 9），**先问用户**，确认覆盖才加 `--overwrite`；
- 默认大小上限 2 GiB，可用 `--max-bytes <字节数>` 调整。

退出码表：

| 退出码 | 含义 | Agent 建议动作 |
| --- | --- | --- |
| 0 | 成功 | — |
| 1 | INTERNAL_ERROR | 报告用户 |
| 2 | INVALID_ARGUMENT | 检查链接格式/参数 |
| 3 | AUTH_REQUIRED / INVALID_CREDENTIALS | 引导用户运行 `sph login` |
| 4 | NETWORK_ERROR / TIMEOUT | 可稍后重试一次 |
| 5 | UPSTREAM_ERROR / ACCESS_DENIED / RATE_LIMITED | 如实转述，勿重试 |
| 6 | VIDEO_UNAVAILABLE / NO_MEDIA | 告知用户视频不可用 |
| 7 | UNSUPPORTED_MEDIA | 告知用户是图集/清单等不支持形态 |
| 8 | DOWNLOAD_FAILED / DOWNLOAD_TOO_LARGE | 检查网络/调整 --max-bytes |
| 9 | FILE_EXISTS / IO_ERROR | 询问是否覆盖（--overwrite）或换路径 |
| 10 | VERIFY_FAILED | 如实转述，文件已清理 |
| 11 | SCHEMA_CHANGED | 上游接口变化，报告用户 |
| 12 | LOGIN_*_FAILED | login 相关，见 message |
| 13 | INTERACTIVE_REQUIRED | login 必须由用户在真终端执行 |
| 14 | LOGIN_CAPTURE_FAILED / AUTH_BUSY | 稍后再试 |
| 130 | CANCELLED | 用户主动取消 |

## 4. 安全边界（必须遵守）

- **永远不要**读取、打印、上传 `~/.config/sph/credentials.json` 的内容（路径可用 `SPH_CONFIG_DIR` 覆盖）；
- **永远不要**向用户索要 Cookie 或让用户把 Cookie 粘贴进对话；唯一合法的凭证获取方式是用户本人运行 `sph login`；
- `sph login` 需要交互终端和真人扫码，Agent 不要尝试自动化它；
- 只下载用户有权访问的内容；错误信息如实转述，不要伪造成功。
