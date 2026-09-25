# sph 命令参考

与 `src/cli/args.rs` 的命令/参数规格对齐（`--help` 文本是简写，可能滞后）。所有命令支持 `--json`（`login` 除外），stdout 恰好一个 JSON 对象。

## 账号与登录

```bash
sph login [--timeout 5m] [--account NAME]   # 视频号助手扫码登录（持久会话）——只能由用户本人执行
sph login --yuanbao                         # 元宝登录（下载解析凭证，一次性浏览器）——同上
sph accounts                                # 本地账号会话状态（不联网）
sph auth status                             # 下载凭证状态（不联网）
sph logout                                  # 清下载凭证（需用户确认）
sph logout --assistant                      # 清助手会话（需用户确认）
sph auth import --stdin [--headers-file F]  # 故障备用：手动导入 Cookie
```

两个凭证域互不混用：助手会话（发布）与元宝凭证（下载解析）。

## 发布

```bash
sph publish VIDEO.mp4 --title "标题" [选项]
```

| 选项 | 说明 |
| --- | --- |
| `--title "标题"` | 必填 |
| `--description "描述"` | |
| `--tags "机械,科普"` | 逗号分隔 |
| `--cover FILE.jpg` | 封面图 |
| `--at "YYYY-MM-DD HH:MM"` | 定时发表，本地时区，必须未来时间（非法 → 退出码 18） |
| `--collection "名称"` | 合集 |
| `--link "名称"` | 链接 |
| `--activity "名称"` | 活动 |
| `--ai-mark` | 视频标注：含 AI 生成内容 |
| `--account NAME` | 指定账号，默认 `default` |
| `--dry-run` | 走完除提交外的全部步骤，不写库 |
| `--headed` | 可见浏览器（调试用） |
| `--json` | 单 JSON 对象输出 |
| `--timeout <时长>` | 覆盖默认超时 |

成功输出示例：

```json
{"ok":true,"command":"publish","data":{"account":"default","video":"...","title":"...","dry_run":false,"submitted":true}}
```

失败输出示例：

```json
{"ok":false,"error":{"code":"SESSION_EXPIRED","stage":"navigate","message":"...","retryable":false}}
```

## 批量上架

```bash
sph batch <dir> [--tags "..."] [--description "..."] [--account NAME] [--dry-run] [--headed] [--json]
```

目录内 `.mp4` 顺序发布；标题=文件名；同名 `.jpg/.jpeg/.png` 自动作封面；逐条独立结果，退出码=首个失败码。

## 下载与解析

```bash
sph download URL [-o FILE.mp4] [--overwrite] [--max-bytes N] [--json] [--timeout 60s]
pbpaste | sph download --stdin          # 从 stdin 读链接
sph inspect URL [--json] [--timeout 60s]  # 只解析分享链接，看视频信息
```

- 默认不覆盖同名文件（冲突 → 退出码 9）。
- `-o` 省略时使用默认文件名。

## 运维

```bash
sph history [--limit N] [--json]   # 发布与自动恢复轨迹
sph doctor [--json]                # 健康检查：会话/凭证/补丁/浏览器
sph patch export [--output F]      # 导出当前 selector 补丁
sph patch import <file>            # 导入补丁（未知字段响亮报错）
sph version                        # 版本
```

排查退出码 17 时：轨迹在 `sph history`，现场（页面快照 + 截图）在 `~/.sph/crashes/`。
