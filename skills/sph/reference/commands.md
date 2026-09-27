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

## 视频管理

```bash
sph list [--collection "名称"] [--limit N] [--account NAME] [--headed] [--timeout 120s] [--json]
sph edit <id> [--title "新标题"] [--description "新描述"] [--cover 3x4.png] [--cover-landscape 4x3.png] [--dry-run] [--account NAME] [--headed] [--timeout 300s] [--json]
```

- `list` 列出已发布视频（视频号助手内部接口，分页拉取）；`--collection` 按合集名精确过滤——**服务端成员接口**（`collection/get_collection_feed_list`，名称先经 `get_collection_list` 解析为 id；合集名不存在 → 退出码 2 并列出可用合集名），`--limit` 上限 200。普通 `list` 输出不含 `collection` 字段（post_list 本身不带合集归属）。
- `edit` 修改已发布视频：`--title` / `--description` / `--cover`（3:4）/ `--cover-landscape`（4:3）**至少一个**（都没有 → 退出码 2）；两张封面可一起传入，`<id>` 来自 `list` 输出（找不到 → 退出码 6）。
- `edit` 的平台机制（2026-09-26 真实校准）：编辑入口是「修改描述和封面」路由页，描述/短标题是**划词编辑**——实现为新旧文本最长公共前后缀 diff 后的一次区间替换：
  - 描述单次最多改 **20 字**、短标题 **16 字**（指差异区间的字数，不是全文长度）；超限 → 退出码 2 并说明当前差异区间大小；
  - `--title` 对应平台「短标题」，总长须 **6..=16 字**（超出 → 退出码 2）；
  - 平台提示「仅支持修改一次，修改后不可撤回，修改记录将会展示在视频上」——实改前务必先 dry-run 并向用户复述；
  - 目标值与当前值相同的字段自动跳过（`changed` 不含它）；全部相同则不提交直接返回。
- `edit` 是**真实对外动作**：默认流程同 publish，先 `--dry-run`（走完定位与表单填写但不提交），确认后再实改。提交后自动重新拉列表复检文本字段；封面内容不在列表接口中，修改封面时 `verified=false` 仅表示无法独立核验，`submitted=true` 表示平台接收修改，应到后台查看两种比例。平台拒绝（如超出可修改次数）→ 退出码 16，不要重试。

成功输出示例：

```json
{"ok":true,"command":"list","data":{"account":"default","count":2,"videos":[{"id":"...","title":"...","collection":"机械系列","created_at":"..."}],"client_side_collection_filter":false}}
{"ok":true,"command":"edit","data":{"account":"default","id":"...","changed":["title"],"dry_run":false,"submitted":true,"verified":true}}
```

## 运维

```bash
sph history [--limit N] [--json]   # 发布与自动恢复轨迹
sph doctor [--json]                # 健康检查：会话/凭证/补丁/浏览器
sph patch export [--output F]      # 导出当前 selector 补丁
sph patch import <file>            # 导入补丁（未知字段响亮报错）
sph version                        # 版本
```

排查退出码 17 时：轨迹在 `sph history`，现场（页面快照 + 截图）在 `~/.sph/crashes/`。
