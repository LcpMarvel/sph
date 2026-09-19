# sph — 微信视频号本地下载工具

[![CI](https://github.com/LcpMarvel/sph-downloader/actions/workflows/ci.yml/badge.svg)](https://github.com/LcpMarvel/sph-downloader/actions/workflows/ci.yml)

命令行工具：打开专用浏览器登录元宝官网一次，之后用本地凭证把视频号分享链接解析并下载成本地 MP4。单机自用，不部署服务器、不开代理、不装证书。

```
sph login                                        # 打开浏览器，扫码，自动保存凭证
sph download "https://weixin.qq.com/sph/xxxx" -o video.mp4
```

- `sph login` 全自动：检测到登录成功后自动采集凭证、关闭浏览器并保存，不需要复制 Cookie、不需要输入任何内容。
- 平时的 `inspect` / `download` 不打开浏览器，适合脚本和 AI Agent 调用。
- 只下载你本来有权访问和保存的内容。

## 安装

从 [Releases](https://github.com/LcpMarvel/sph-downloader/releases) 下载对应平台的压缩包，解压即用：

| 文件 | 平台 |
| --- | --- |
| `sph-darwin-arm64.zip` | macOS Apple Silicon |
| `sph-darwin-amd64.zip` | macOS Intel |
| `sph-linux-amd64.tar.gz` | Linux x86_64 |
| `sph-linux-arm64.tar.gz` | Linux ARM64 |
| `sph-windows-amd64.zip` | Windows x64 |
| `sph-windows-arm64.zip` | Windows ARM64 |

或者从源码构建（Go ≥ 1.23）：

```bash
go build -o bin/sph ./cmd/sph
```

## 环境要求

- macOS / Linux / Windows；`sph login` 需要桌面会话（要弹浏览器扫码）
- 可选：`ffprobe`（如 `brew install ffmpeg`）——下载后做视频流验证；没有它只做 MP4 容器基础检查

运行时零外部依赖：不需要 Node、不需要预装浏览器。首次 `sph login` 会自动下载一个专用 Chromium（约 200MB，仅一次，之后复用）。

## 使用

```bash
# 登录：打开专用浏览器 → 扫码/认证 → 自动保存并关窗
./bin/sph login [--timeout 5m]

# 查看本地凭证状态（不联网，不显示任何值）
./bin/sph auth status

# 解析（不下载）
./bin/sph inspect "https://weixin.qq.com/sph/xxxx" [--json]

# 下载
./bin/sph download "https://weixin.qq.com/sph/xxxx"                 # 自动命名: 标题_标识.mp4
./bin/sph download "https://weixin.qq.com/sph/xxxx" -o video.mp4    # 指定路径
./bin/sph download "https://weixin.qq.com/sph/xxxx" -o v.mp4 --overwrite --max-bytes 1073741824
echo "https://weixin.qq.com/sph/xxxx" | ./bin/sph download --stdin

# 登录失效后重新登录（下载过程永远不会自动弹浏览器）
./bin/sph login

# 清除本工具保存的本地凭证（不影响浏览器里的登录）
./bin/sph logout
```

同目录已有同名文件时默认报 `FILE_EXISTS`（退出码 9），不会覆盖；加 `--overwrite` 才替换。

## 给脚本 / AI Agent 用（Codex、Claude Code 等）

除 `login` 外所有命令都是**非交互**的，天然适合 Agent 与脚本调用：

- `--json` 模式下 stdout **恰好一个 JSON 对象**（进度和诊断全在 stderr），可以直接解析；
- 退出码稳定可编程（见下表），Agent 可以按码决定重试/报错/提示用户重新登录；
- 失败信息永远是安全的（不含 Cookie、token、签名 URL），可以原样转述给用户。

```bash
sph inspect  "https://weixin.qq.com/sph/xxxx" --json
# {"ok":true,"command":"inspect","data":{"local_id":"...","title":"...","author":"...","media_source":"h264VideoInfo","codec_hint":"h264"}}

sph download "https://weixin.qq.com/sph/xxxx" -o out.mp4 --json
# {"ok":true,"command":"download","data":{"local_id":"...","path":"/abs/out.mp4","bytes":123,"sha256":"...","verification":"ffprobe"}}

# 失败时：
# {"ok":false,"error":{"code":"AUTH_REQUIRED","stage":"credentials","message":"未找到登录凭证，请执行 sph login 重新登录。","retryable":false}}
```

错误对象字段：`code`（机器可读错误码）、`stage`（出错阶段：arguments / credentials / parse_share / fetch_feed / select_media / download / verify / commit / login_*）、`message`（安全的中文说明）、`retryable`。

退出码表：

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功 |
| 1 | INTERNAL_ERROR 未预期内部错误 |
| 2 | INVALID_ARGUMENT 参数/链接不合法 |
| 3 | AUTH_REQUIRED / INVALID_CREDENTIALS 缺少凭证或凭证失效（提示重新 login） |
| 4 | NETWORK_ERROR / TIMEOUT 网络失败或超时 |
| 5 | UPSTREAM_ERROR / ACCESS_DENIED / RATE_LIMITED 上游业务失败、403、限流 |
| 6 | VIDEO_UNAVAILABLE / NO_MEDIA 视频不可用或无媒体 |
| 7 | UNSUPPORTED_MEDIA 图集、HLS/DASH 等不支持形态 |
| 8 | DOWNLOAD_FAILED / DOWNLOAD_TOO_LARGE 媒体下载失败或超过大小上限 |
| 9 | FILE_EXISTS / IO_ERROR 目标已存在或写入失败 |
| 10 | VERIFY_FAILED 容器/ffprobe 验证未通过 |
| 11 | SCHEMA_CHANGED 上游接口结构变化 |
| 12 | LOGIN_DEPENDENCY_MISSING / LOGIN_BROWSER_FAILED login 依赖缺失或浏览器失败 |
| 13 | INTERACTIVE_REQUIRED login 需要真终端 |
| 14 | LOGIN_CAPTURE_FAILED / AUTH_BUSY 凭证采集失败或凭证修改锁被占用 |
| 130 | CANCELLED 用户取消 |

注意：`login` 必须由人在真实终端执行（要扫码），Agent 不要尝试自动化它；Agent 所在机器没有凭证时，应提示用户登录或使用下述服务器方式。

## 在服务器上使用（无图形界面）

服务器跑不了 `sph login`（需要可见浏览器和人工扫码）：

1. 在自己的 Mac 上完成 `sph login`；
2. 把 `~/.config/sph/credentials.json` 拷到服务器的同一路径（或用 `SPH_CONFIG_DIR` 指向所在目录）；
3. 服务器上只需要 `bin/sph` 一个二进制，直接 `inspect` / `download`。

也可以用故障备用入口直接导入 Cookie：在日常浏览器登录 yuanbao.tencent.com，开发者工具 Network 里找到 `get_parse_result` 请求，复制其完整 Cookie 值：

```bash
pbpaste | ./bin/sph auth import --stdin
# 可选：同会话额外请求头（白名单字段，JSON 键值对象）
pbpaste | ./bin/sph auth import --stdin --headers-file "$HOME/.config/sph/yuanbao-headers.json"
```

凭证文件是 0600 明文 JSON，请按密钥对待，不要提交到仓库或发给任何人（包括 AI）。

## 安全与隐私

- 凭证只存在 `~/.config/sph/credentials.json`（目录 0700、文件 0600、原子写入），不上传任何地方。
- 元宝 Cookie/会话头只发给元宝解析端点；视频号预览与媒体服务器收不到任何会话信息。
- 输出（含 JSON、日志）不含 Cookie、token、带签名的媒体 URL；媒体 URL 原样使用不改签名参数。
- 下载为单连接流式写入，默认上限 2 GiB，临时文件在验证通过后才原子提交。
- `login` 每次用一次性私有浏览器 profile，结束即删除，不碰日常浏览器。
- Go 客户端直连（忽略代理环境变量），拒绝回环/私网地址。

## 常见问题

- `LOGIN_BROWSER_FAILED`：登录浏览器启动失败；首次使用需要联网下载浏览器，检查网络后重试。
- `INTERACTIVE_REQUIRED`：login 必须在真终端运行。
- `INVALID_CREDENTIALS`：凭证失效，重新 `sph login`。
- `VERIFY_FAILED`：文件容器/ffprobe 检查未过——可能截断、接口变化或该视频需要本工具未实现的解码。
- 下载中想中断：Ctrl+C，不会留下伪完成文件。

## 卸载

`./bin/sph logout` 清除凭证后删除项目目录即可。强杀进程可能残留 `~/.config/sph/.login-*` 目录，按提示路径手动删除。

## 开发

```bash
make build     # go build -o bin/sph ./cmd/sph
make test      # go test ./...
make check     # gofmt 检查 + go vet
make race      # go test -race ./...
```

发布新版本：推送 `v*` 标签即可触发构建并发布 Release（六个平台产物 + SHA-256 校验和）：

```bash
git tag v1.0.1 && git push origin v1.0.1
```

结构：`cmd/sph` 入口；`internal/cli` 参数与输出契约；`internal/auth` 凭证存取与锁；`internal/login` 登录编排（go-rod 驱动专用浏览器 + Cookie 观察状态机）；`internal/upstream` 两步解析链；`internal/netpolicy` 网络边界；`internal/download` 流式下载与原子提交；`internal/verify` 容器与 ffprobe 检查。

依赖：Go 标准库 + [go-rod/rod](https://github.com/go-rod/rod)（MIT，浏览器自动化）。
