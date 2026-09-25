# sph PRD v2 — 视频号本地自动化 CLI / Agent Runtime

> 版本：v2.1（定位升级 + 技术栈决策） · 日期：2026-09-24 · 状态：草案
> 取代 v1「微信视频号本地下载工具」定位；下载能力保留，降级为众多 command 之一。
> 已决策：实现语言从 Go 全量切换为 **Rust**；v1 凭证不做迁移，用户在 v2 重新授权一次。

---

## 1. 一句话定位

> **sph is a local-first CLI and agent runtime for automating WeChat Channels (视频号), including login, download, publishing, scheduling and account operations.**

sph = ShiPinHao。定位升级后这个名字反而更准确：它不再是"下载器"，而是**视频号的本地 Agent 基础设施**。

## 2. 背景：为什么升级

v1 验证了三个关键假设：

1. 单二进制、local-first、无服务端的形态成立（Go，六平台 Release）；
2. 浏览器登录可以全自动化（扫码检测 → 凭证采集 → 自动关窗）；
3. 非交互 + JSON + 稳定退出码的契约让 AI Agent 可以可靠地把 sph 当工具调用。

但"下载"只是创作者链路的一环。真实工作流是：

```text
登录 → 找素材/下载 → 发布 → 排期 → 看数据 → 互动
```

其中"发布"是最高频、最重复、最适合自动化、且目前完全没有本地工具的动作。同时 AI Agent（Claude Code / Codex / WorkBuddy）需要一个**本地的、确定性的视频号操作入口**——这正是 sph 的自然延伸。

## 3. 目标与非目标

### 目标

- **统一浏览器会话**：一次扫码登录视频号助手，持久化 Chromium profile，之后所有命令复用登录态。
- **发布自动化**：`sph publish` 用确定性浏览器流程完成 上传 → 元信息 → 封面 → 声明 → 提交。
- **分层恢复**：固定流程优先，失败才逐级升级（固定流程 → Jev → LLM），**LLM 永不进入主流程**。
- **Agent 一等公民**：除 `login` 外全部非交互；`--json` 单对象输出；退出码稳定可编程；M4 暴露 MCP。
- **local-first 不变**：单二进制、无服务端、凭证不出本机。

### 非目标

- 不做云服务、不做多端同步、不做部署形态。
- 不做矩阵营销、刷量、批量养号等违反平台规则的能力；只自动化**用户本人有权操作的账号**。
- 不逆向私有协议：所有浏览器侧操作走真实页面，与人工操作同路径。
- M1 不做：定时、批量、多账号、数据统计、评论、MCP。

## 4. 用户与场景

| 用户 | 场景 |
| --- | --- |
| 个人创作者 | 本地剪好的视频 → `sph publish ./v.mp4 --title ... --tags ...`，不开网页 |
| 内容运营 | `sph batch ./videos/` 批量上架；`--at` 定时发表；`sph status` 查队列 |
| 素材收集者 | `sph download <url>`（v1 能力，原样保留） |
| AI Agent | 把 sph 当本地工具：`sph publish ... --json`，按退出码决策；M4 起直接 `sph mcp` 挂载 |

示例（Agent 场景）：

```text
用户: "把桌面上剪好的三条视频发到我的视频号，标题按文件名，今晚 8 点定时"
Agent: sph publish ... --at "20:00" --json ×3 → 解析退出码 → 汇报结果
```

## 5. 设计原则

1. **Local-first**：一切状态在 `~/.sph/`，二进制拷走即用。
2. **确定性优先**：固定浏览器流程覆盖 90% 情况，速度、成本、稳定性都由此决定。
3. **失败分级恢复**：只有 selector 失效 / 弹窗变化 / 页面改版 / 未知状态才进入恢复层。
4. **会话复用**：浏览器 profile 持久化，任何命令不重复扫码；下载永不弹浏览器（v1 承诺延续到所有命令）。
5. **人机同构**：每个命令既能人用，也能被脚本 / Agent 无差别调用。
6. **安全边界**：凭证 0600、输出无敏感信息、媒体与凭证分域（延续 v1 安全模型）。

## 6. 命令面（CLI Surface）

```bash
# ── 账号（M1 起）──
sph login                     # 打开真实浏览器扫码，保存持久化 Chromium profile
sph accounts                  # 列出本机账号档案（M3 多账号完善）
sph logout                    # 清除指定账号的本地会话与凭证

# ── 下载（v1 已有，原样保留）──
sph download <url> [-o out.mp4] [--json]
sph inspect  <url> [--json]
sph auth status

# ── 发布（M1 核心新增）──
sph publish ./video.mp4 \
    [--title "..."] [--description "..."] [--tags "机械,科普"] \
    [--cover ./cover.jpg] \
    [--dry-run] [--headed] [--json]

# ── 定时与批量（M3）──
sph publish ./video.mp4 --at "2026-09-25 20:00"
sph batch ./videos/ [--concurrency 1]

# ── 状态（M3）──
sph status                    # 当前队列 / 进行中任务
sph history                   # 历史操作记录（history.db）

# ── 调试（M1 起）──
sph publish ./v.mp4 --headed  # 可视化浏览器，观察自动化过程
sph publish ./v.mp4 --dry-run # 走完除"提交"外的全部步骤并报告

# ── 远期（M4+，占位不承诺）──
sph list                      # 最近作品
sph stats                     # 播放 / 点赞 / 评论数据
sph comments                  # 读取评论
sph reply <id> "谢谢"          # 回复评论
sph mcp                       # 以 MCP server 暴露全部能力
```

## 7. 架构

### 7.1 四层结构

```text
sph
│
├── session/          # 账号与会话（M1）
│   ├── login         # 扫码登录编排
│   ├── profile       # 持久化 Chromium profile 管理
│   └── accounts      # 账号档案登记 / 切换
│
├── download/         # 视频解析 + 下载（v1 已有，原样保留）
│
├── publish/          # 发布流水线（M1）
│   ├── upload        # 视频上传
│   ├── metadata      # 标题 / 描述 / 标签
│   ├── cover         # 封面
│   ├── declaration   # 原创声明等勾选项
│   ├── schedule      # 定时发表（M3）
│   └── submit        # 最终提交（dry-run 在此止步）
│
└── browser/          # 自动化基础设施
    ├── runtime       # 浏览器运行时（CDP 客户端 + Chromium 下载）
    ├── page_state    # 页面状态快照（selector map / DOM 摘要 / 截图）
    ├── jev_recovery  # Jev 恢复层（M2）
    └── llm_fallback  # LLM 兜底（M2）
```

### 7.2 核心机制：分层恢复（这是整个项目的设计支点）

```text
正常路径（90% 情况）:
  确定性浏览器流程（固定 selector / 等待条件，CDP 驱动）──→ 直接成功

失败时逐级升级:
  固定流程失败
      ↓ 采集 page state（快照 + 失败上下文）
  Jev：理解页面状态 → 选择 action + element → 交回浏览器执行
      ↓ 仍失败
  LLM（Claude / Codex）：理解异常 → 给出恢复策略
      ↓ 仍失败
  报错退出 + 保存现场（截图 / DOM / 恢复轨迹），提示人工介入
```

反模式（明确禁止）：

```text
每一步 → LLM        # 慢、贵、不稳定，本项目不采用
```

约束：

- LLM / Jev 只出现在 failure 路径，主流程零 LLM 依赖；
- **不引入 browser-use 类通用浏览器 agent 框架**：Python 运行时与单二进制形态冲突；其 LLM-in-the-loop 模型与本原则相悖；恢复层只需一次结构化 LLM 调用（快照 → `{action, selector}`），几百行代码即可，不需要 agent loop。其 DOM 蒸馏思路（带序号的可交互元素清单）可借鉴用于 page_state 设计；开发期可作一次性页面探索工具，不进运行时依赖。
- 每次恢复记录完整轨迹（页面状态、决策、动作、结果）写入 history.db，作为后续自愈和回归的素材；
- （M3 可选）Jev/LLM 的成功修复沉淀为 selector 补丁，下次同页面直接命中——页面小改版不再需要模型介入。

### 7.3 统一浏览器会话与数据目录

```text
~/.sph/
├── accounts/
│   ├── <account-a>/
│   │   ├── profile/        # 持久化 Chromium profile（登录态）
│   │   └── config.json     # 账号档案（昵称、ID、创建时间）
│   └── <account-b>/
├── history.db              # 全部操作记录（SQLite）
└── config.toml             # 全局配置
```

- 首次 `sph login` 打开真实浏览器 → 扫码 → 保存 profile；之后 `download` / `publish` / 未来一切命令复用同一登录态。
- 浏览器二进制沿用 v1 策略：首次自动下载专用 Chromium，多镜像竞速，国内自动走 npmmirror。
- **不迁移凭证**：v2 首次使用按 `AUTH_REQUIRED` 提示重新 `sph login`（元宝重新授权一次即可），不写 v1 凭证迁移代码；`~/.config/sph` 旧文件保留不删，用户自行清理。`SPH_CONFIG_DIR` 语义延续。

### 7.4 会话域（重要：两个凭证域，互不混用）

| 域 | 用途 | 形态 | 引入 |
| --- | --- | --- | --- |
| 视频号助手会话（channels.weixin.qq.com） | publish / list / stats / comments | 持久化 Chromium profile | M1 |
| 元宝解析凭证 | download / inspect | credentials.json（0600） | v1 已有 |

下载链路（元宝解析 + Go 直连下载）**保持不变**——它快、稳、不需要浏览器。视频号助手会话服务于发布及未来的账号操作。两个域在 `accounts` 层统一登记，但凭证各自独立存储、独立过期、独立提示重新登录。

### 7.5 发布流水线

```text
load session → 打开发布页
    → upload      （校验：上传完成态）
    → metadata    （title / description / tags，校验：字段回填成功）
    → cover       （上传或截取，校验：封面就绪）
    → declaration （原创声明等，默认保守：不勾原创）
    → submit      （dry-run 在此停止并输出报告）
```

每一步：`执行 → 校验 page state → 失败进入 7.2 恢复层`。任何一步失败，整体失败并保存现场，不产生"半发布"状态。

### 7.6 history.db

每次命令一条记录：`command / account / 输入参数（脱敏）/ 结果 / 错误码 / 耗时 / 恢复轨迹 / 产物路径`。它是 `sph status`、`sph history`、批量队列和未来自愈回归的统一数据源。

### 7.7 技术栈：Rust 全量重写

v2 实现语言定为 **Rust**；Go 版 v1 冻结保留（git tag 与已发布 Release 继续可用，作为协议与契约的权威参考）。

- 形态不变：单一静态二进制、六平台分发、零外部运行时依赖（ffprobe 仍为可选外部校验工具）。
- 浏览器自动化：Rust 无官方 Playwright 绑定，改为 **CDP 直连**——选型 [chromiumoxide](https://github.com/mattsse/chromiumoxide) 0.9（tokio 运行时，与依赖栈一致），其上自建薄封装（固定 selector / 等待原语 / page state 快照）。**覆盖度 spike 已通过（7/7，代码在 `spikes/chromiumoxide/`）**：headless/headed 启动、页面事件订阅、Cookie 轮询、DOM/JS、文件上传、浏览器下载落盘全部可用。**确定性优先、失败分级恢复的架构原则与语言无关，不变。**
- 下载链移植 = 机械翻译：元宝解析契约、2xx 语义、登录三段式状态机、错误码表均有权威记录，Go 版离线测试 fixture 照搬，**不重新做协议验证**。
- 依赖哲学沿用 v1：clap（CLI）、tokio + reqwest（HTTP）、rusqlite（history.db）、serde；保持依赖清单短。
- 浏览器二进制策略不变：首次自动下载专用 Chromium，多镜像竞速，国内自动走 npmmirror。

chromiumoxide 使用注意（spike 实测）：

- 无 `Network.getAllCookies`——用 `Storage.getCookies`（返回浏览器全部 Cookie）做登录状态机的轮询原语；
- `Element` 无 `set_input_files` 封装——经 `el.node_id` + 裸 `DOM.setFileInputFiles` 调用；
- 枚举命名带方法前缀（如 `SetDownloadBehaviorBehavior`）；部分 Returns 为空结构（如 `SetCookieReturns` 无 `success`），以行为断言为准。

## 8. 错误契约扩展

v1 退出码表（0–14, 130）全部保留。新增：

| 退出码 | 含义 |
| --- | --- |
| 15 | SESSION_EXPIRED 视频号助手登录态失效（提示重新 `sph login`） |
| 16 | PUBLISH_REJECTED 平台侧校验不通过（声明缺失 / 类目限制 / 内容审核提示） |
| 17 | RECOVERY_FAILED Jev 与 LLM 均未恢复（现场已保存） |
| 18 | SCHEDULE_INVALID 定时参数非法或平台定时入口不可用（M3） |

`stage` 新增：`session_load / navigate / upload / metadata / cover / declaration / schedule / submit / recovery`。

错误信息延续 v1 约束：中文、安全、不含 Cookie/token/签名 URL，Agent 可原样转述。

## 9. 里程碑

### M1 — Rust 重写达到 v1 等价 ✅ 代码完成（2026-09-24）

- clap 命令骨架：`login / logout / auth status / inspect / download`，`--json` 契约与退出码表逐一对齐 v1；
- 元宝登录（chromiumoxide + 三段式状态机移植）与解析/下载链移植；Go 版离线测试 fixture 照搬；
- `~/.sph/` 目录结构（config.toml + history.db 落库）；不迁移 v1 凭证，提示重新授权；
- 六平台交叉编译 + Release 工作流（tag `v2.*`）。
- **验收**：v1 离线测试全量移植通过；契约 diff 为空。
- **浏览器层选型已定**：chromiumoxide 0.9（spike 7/7 通过，见 `spikes/chromiumoxide/`）。
- **实测状态**：`cargo test` 80/80 通过（apperr/media/netpolicy/auth/upstream/download/verify/login/cli 全模块）；clippy 零警告；release 冒烟通过（version/auth status/--json 信封/退出码与 v1 一致）。
- 移植中修复的真实 bug：mpsc 发送者退出（EOF）会被 `select!` 误判为手动触发——Go 的 channel 从不关闭所以无此问题（`manual_open` 标记修复）。
- 待办：CI 首跑验证（aarch64-linux/Windows 交叉交给 CI）；真实 login/下载端到端验收（需用户扫码参与）。

### M2 — 发布：视频号助手 login + `publish` ✅ 代码完成（2026-09-24）

- 视频号助手扫码登录 + 持久化 Chromium profile（`~/.sph/accounts/default/profile/`，登录态即 profile，不采集 cookie）；
- `sph publish` 支持：`video / title / description / tags / cover`，全确定性流程（upload → metadata → cover → declaration → submit）；
- `--json` / `--dry-run` / `--headed` / `--account`；新错误码 15 SESSION_EXPIRED / 16 PUBLISH_REJECTED 落地（17 预留给 M3）；
- 命令分派定案：`sph login` 默认登录助手持久会话，`sph login --yuanbao` 保留 v1 元宝凭证路径（下载域）；`sph logout --assistant` 清助手会话，裸 `logout` 保持 v1 语义（清下载凭证）；`sph accounts` 双域状态一览；
- selector 集中为 `publish::page::Selectors` 常量表（DEFAULT_SELECTORS），**已用真实页面校准**（2026-09 实测）；M3 补丁机制接管改版；
- **实测状态**：`cargo test` 98/98——发布流水线 6 个真实 Chromium e2e（fixture 页面：happy/dry-run/缺会话/登录页/平台拒绝/默认勾选原创）全部离线通过；clippy 零警告；冒烟符合契约。
- 实现备注：表单输入用 DOM 赋值 + input/change 事件（受控表单标准驱动，规避 type_str 对中文按 key 名查表失败）；boolean 勾选态用 property() 而非 string_property()。
- **验收完成（2026-09-24）**：真实发布成功——上传 → 元信息 → 话题 → 声明核对 → 提交，平台跳转确认（post/list）。提交成功判定 = 发表后页面跳转（校准自 frankwei2019/auto-weixin-video）；发表按钮禁用态等待已加。
- 待办：README / agent-guide 按新定位重写（随下个 Release）。

### M3 — 自愈：恢复层 + 补丁机制（第一刀 ✅ 2026-09-24）

- **补丁机制**：`~/.sph/patches/publish.json` 覆盖 selector（只列改动的字段；未知字段名/坏 JSON 响亮报错）——修复单元从"改源码发版"变成"运行时 JSON"，M2 校准经验直接沉淀为能力；
- **page_state 快照**：全根（wujie shadow + 主文档）可交互元素蒸馏清单（带序号，browser-use 式）+ URL + stage + 失败 selector；
- **现场保存**：失败时 `~/.sph/crashes/<ts>/{snapshot.json,page.txt,screenshot.png}`；
- **RecoveryBackend 接口**：`recover(snapshot) -> Option<RecoveryAction{Click/Type/Wait}>`；默认 NullBackend（人工终态）；Jev/本地模型/远程 LLM 实现同一 trait 即可接入——运行时永远确定性，模型只产出一次结构化动作；
- **恢复钩子**：流水线每步失败 → 快照 → 现场 → 后端决策 → 执行+重试一次 → 仍失败报 RECOVERY_FAILED(17)；
- **恢复轨迹**：`~/.sph/history.jsonl` 追加（审计与回归素材；SQLite history.db 推迟到需要查询时）；
- **实测**：110/110 测试（含失败现场保存 e2e：超时 → crashes 落盘 + 轨迹入 history.jsonl）。
- 待办：Jev/LLM 后端实现（接口已就绪）；补丁 export/import/report；`sph doctor`。

### M4 — 规模化：`--at` / `batch` / 多账号 ✅（2026-09-24）

- **`--at "YYYY-MM-DD HH:MM"` 定时发表**：radio"定时" → 日期 picker（读头部月份翻页、点目标日）→ 时间输入（focus+select+insertText+Tab blur，React 18 受控组件的键盘级路径）→ 回填校验；交互序列校准自 frankwei2019/auto-weixin-video 的踩坑记录；非未来时间/坏格式报 18 SCHEDULE_INVALID；
- **`sph history [--limit N] [--json]`**：读取恢复轨迹 history.jsonl（M3 落盘），最近 N 条 / JSON 信封；
- **M4 完成（2026-09-24 第二刀）**：
  - `login --account NAME`：多账号登录就位；
  - `sph batch <dir>`：目录内 .mp4 顺序发布（标题=文件名，同名 .jpg/.jpeg/.png 自动作封面；逐条结果汇总，退出码=首个失败码）；
  - **`sph doctor [--json]`**：健康检查（账号会话/下载凭证/补丁有效性/浏览器可用性），fail 退出 1；
  - **`sph patch export/import`**：补丁打包与导入（import 校验字段名，坏补丁响亮拒绝）；
- **实测**：122/122 测试（batch dry-run 双视频 e2e、patch 导出导入 roundtrip、坏补丁拒绝、doctor 空/就绪两种环境）。

### M3 后半 ✅（同日）

- **RuleBackend**（Jev 的确定性形态）成为 publish 默认恢复后端：阻塞对话框（我知道了/确定/切换/同意/关闭）→ 点击；页面近乎空白 → 等待重试——探针阶段真实被拦的模式，零外部依赖；
- **恢复语义收窄**：只有页面状态类失败（Timeout/SchemaChanged/SessionExpired）走恢复重试；平台明确拒绝（PUBLISH_REJECTED 等）直接返回原始错误（重试无意义）；
- LLM 后端接口已就绪但不做远程 LLM（零密钥零依赖立场；本地模型后端留待真有需求时经同一 trait 接入）。
- 待办（低优先）：真实定时验收（日常使用验证）、Jev 本地模型后端、补丁社区分发（有真实共享需求再做）。

### M6 — 扩展属性 ✅（2026-09-24）

- `--collection` / `--link` / `--activity`（通用下拉选择：label 定位 → 展开弹层 → 文案匹配点选 → 不存在则响亮报错）；
- `--ai-mark` 视频标注"含 AI 生成内容"（div 模拟 checkbox：完整 mouse 事件序列 + click，校验 is-selected；校准自 awv 踩坑记录）；
- 修复 page_state 解析 bug（DISTILL_JS 返回 JSON 字符串未二次解析——现场 URL/元素自 M3 起一直为空，修复后 RuleBackend 首次拿到真实元素）；
- 修复未登录检测（URL 级 login/passport 判定，会话过期 5 秒内报 15 不再等满超时）；
- 项目改名 LcpMarvel/sph；README/agent-guide 按 v2 重写；CI 修 Ubuntu chromium snap 空壳问题；
- 124/124 测试；
- **真实验收（2026-09-25 凌晨）**：`--at` 定时发布全链路真实平台通过（定时 5 小时后，提交+跳转确认）——但暴露并修复两个接线 bug：`--at`/扩展属性的 CLI→Options 赋值与流水线 4.5 步骤当时未真正落盘（python 补丁静默失败，e2e 绕开 CLI 未暴露）；AI 标注（--ai-mark）曾被误判为合成事件过滤：真实控件 `selectOption` 点一次选中、再点一次取消，旧实现在事件序列的 click 之后又 `el.click()`，第二次把选中清掉（下拉收起、校验失败）。改为只派发一次 click 后，真实页面可选中。
- 125/124→125/125 测试（新增 ai_mark-only e2e）。

- 定时发表（优先平台原生定时入口，不可用时本地调度兜底）；
- `sph batch <dir>`；多账号登记与 `--account` 切换；
- `sph status` / `sph history` 完整化。

### M5 — ~~`sph mcp`~~ 已砍（2026-09-24 用户决策）

不做 MCP server。理由：`--json` 单对象输出 + 稳定退出码 + 完整错误契约已经是 Agent 集成的最佳形态
（v1 的 agent-guide 即此结论），MCP 只是等价包装，不值得维护成本。
Agent 直接 `sph publish ... --json` 即可。若未来有强烈需求再议。

## 10. 风险与开放问题

| 风险 / 问题 | 应对 |
| --- | --- |
| ~~Rust 浏览器自动化生态弱于 Playwright~~ | **已解决**：spike 7/7 通过（`spikes/chromiumoxide/`），chromiumoxide 0.9 定为浏览器层；维护节奏风险由薄封装隔离 |
| 视频号助手页面改版导致固定流程失效 | 分层恢复就是为此设计；另需真实页面定期回归机制（M3 起） |
| 扫码会话有效期与风控 | SESSION_EXPIRED 契约 + 明确重新登录提示；不尝试绕过风控 |
| 平台审核与内容合规 | sph 只自动化"提交动作"，内容合规由用户负责；声明类选项默认保守 |
| 定时发表依赖平台功能存在 | M4 先做平台原生，本地兜底为降级方案 |
| Jev 的具体形态未定 | M3 前完成技术选型，不阻塞 M1/M2 |
| 双会话域增加用户理解成本 | `sph accounts` / `sph auth status` 统一展示，文档说清"下载用解析凭证、发布用助手会话" |

## 11. 与 v1 的兼容

- **实现整体替换**：v2 为 Rust 重写；Go 版代码冻结于 v1.x tag，已发布 Release 继续可用。同一仓库内进行，历史与 Release 保留；仓库名暂不改名。
- **命令契约不变**：命令名、参数、`--json` 输出结构、退出码表逐一对齐 v1——Agent 与脚本集成无感切换。
- **凭证不迁移**：v2 首次使用提示重新 `sph login`（元宝重新授权一次）；`~/.config/sph` 旧文件保留不删，由用户自行清理。
- 版本线：v2.0.0 起跳，tag `v2.*` 触发 Release。
- README 首屏随 M2 按新定位重写；品牌口径：`sph` 指整个 CLI，「下载器」只描述 `sph download`。
