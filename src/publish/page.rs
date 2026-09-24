//! 发布页对象：校准后的真实平台 selector + wujie 子应用感知。
//!
//! 平台是无界（wujie）微前端：真实 UI 在 `wujie-app` 的 shadowRoot 内。
//! 所有 DOM 操作的查询顺序：逐个 wujie 子应用查找 → 回退主文档，
//! 因此离线 fixture（普通 DOM）与真实平台共用同一套代码。
//!
//! selector 表集中于此（2026-09 校准自真实页面；改版后由 M3 补丁机制接管）。

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::dom::{
    EnableParams, GetDocumentParams, GetSearchResultsParams, NodeId, PerformSearchParams,
    SetFileInputFilesParams,
};
use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
use chromiumoxide::Page;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::wait_for_selector;
use crate::upstream::sanitize_message;

const STEP_TIMEOUT: Duration = Duration::from_secs(90);

/// 发布页 selector 表（校准自 channels.weixin.qq.com 真实 DOM）。
#[derive(Debug, Clone)]
pub struct Selectors {
    /// 主页就绪标志（登录后主页品牌元素，存在即视为已登录）。
    pub home_ready: Cow<'static, str>,
    /// 登录页标志（存在即未登录 → SESSION_EXPIRED）。
    pub login_indicator: Cow<'static, str>,
    /// 主页"发表视频"入口按钮文案（子应用内）。
    pub publish_entry: Cow<'static, str>,
    /// 侧边栏"视频"菜单项（进入视频管理页）。
    pub video_menu: Cow<'static, str>,
    /// 视频文件上传框（DOM.performSearch 查询串，穿透 shadow root）。
    pub video_file_input: Cow<'static, str>,
    /// 上传完成标志：封面预览容器出现（平台自动截帧）。
    pub upload_done_indicator: Cow<'static, str>,
    /// 短标题输入框。
    pub title_input: Cow<'static, str>,
    /// 视频描述编辑器（contenteditable）。
    pub description_editor: Cow<'static, str>,
    /// 话题前缀（在描述编辑器内联输入 #话题 + 空格触发自动完成）。
    pub topic_prefix: Cow<'static, str>,
    /// 封面上传框（图片文件框，performSearch 查询串）。
    pub cover_file_input: Cow<'static, str>,
    /// 原创声明勾选框（"声明原创" label 所在表单项内）。
    pub original_declaration_checkbox: Cow<'static, str>,
    /// "声明原创"文案（用于定位原创表单项）。
    pub original_declaration_label: Cow<'static, str>,
    /// 发表按钮文案。
    pub submit_button: Cow<'static, str>,
    /// 发表成功标志。
    pub publish_success_indicator: Cow<'static, str>,
    /// 平台错误/拒绝提示。
    pub publish_error_indicator: Cow<'static, str>,
}

/// 内置 selector（2026-09 真实页面校准）。
pub static DEFAULT_SELECTORS: Selectors = Selectors {
    home_ready: Cow::Borrowed(".brand-name"),
    login_indicator: Cow::Borrowed(".qrcode-tip, #qrcode-login, .login-qrcode"),
    publish_entry: Cow::Borrowed("发表视频"),
    video_menu: Cow::Borrowed(".finder-ui-desktop-menu__sub__li"),
    video_file_input: Cow::Borrowed("input[type=file][accept*='video']"),
    upload_done_indicator: Cow::Borrowed(".cover-preview-wrap"),
    title_input: Cow::Borrowed("input[placeholder*='短标题']"),
    description_editor: Cow::Borrowed(".input-editor"),
    topic_prefix: Cow::Borrowed("#"),
    cover_file_input: Cow::Borrowed("input[type=file][accept*='image']"),
    original_declaration_checkbox: Cow::Borrowed(".ant-checkbox-input"),
    original_declaration_label: Cow::Borrowed("声明原创"),
    submit_button: Cow::Borrowed("发表"),
    publish_success_indicator: Cow::Borrowed(".publish-success, #result[data-value='published']"),
    publish_error_indicator: Cow::Borrowed(
        ".weui-desktop-dialog__bd, .forbid-dialog-title, .publish-error",
    ),
};

/// 子应用定位前缀：找到第一个含 __probe 选择器的 wujie 子应用文档，
/// 找不到回退主文档（离线 fixture 即走此路径）。
/// 全根遍历前缀：__roots 含所有 wujie 子应用文档与主文档。
/// 平台弹窗与入口可能渲染在任意一层（wujie 水合前弹窗在主文档骨架）。
const ROOTS: &str = r#"const __roots = [...document.querySelectorAll('wujie-app')]
          .map(a => a.shadowRoot)
          .filter(Boolean);
        __roots.push(document);"#;

pub struct PublishPage<'a> {
    page: &'a Page,
    pub selectors: &'a Selectors,
    dom_enabled: std::cell::Cell<bool>,
}

impl<'a> PublishPage<'a> {
    pub fn new(page: &'a Page, selectors: &'a Selectors) -> PublishPage<'a> {
        PublishPage {
            page,
            selectors,
            dom_enabled: std::cell::Cell::new(false),
        }
    }

    /// 未登录检测（主文档）。
    pub async fn is_login_page(&self) -> bool {
        self.page
            .find_element(self.selectors.login_indicator.as_ref())
            .await
            .is_ok()
    }

    /// 校验当前仍处于已登录态（登录页标志出现即会话失效）。
    pub async fn require_logged_in(&self) -> Result<()> {
        if self.is_login_page().await {
            return Err(AppError::new(
                Code::SessionExpired,
                Stage::Navigate,
                "视频号助手会话已失效，请执行 sph login 重新登录。",
            ));
        }
        Ok(())
    }

    /// 主页就绪检测（登录流程使用）。
    pub async fn wait_home_ready(&self, timeout: Duration) -> Result<()> {
        wait_for_selector(
            self.page,
            self.selectors.home_ready.as_ref(),
            timeout,
            Stage::Navigate,
        )
        .await
    }

    /// 点掉子应用内可见的引导对话框（我知道了/确定/切换）。
    pub async fn dismiss_dialogs(&self) -> Result<bool> {
        let js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          let clicked = false;
          for (const sub of __roots) {{
            sub.querySelectorAll('button').forEach(b => {{
              const t = (b.innerText||'').trim();
              if (vis(b) && ['我知道了','确定','好的','切换'].includes(t)) {{
                b.dispatchEvent(new MouseEvent('click', {{bubbles:true, cancelable:true}}));
                clicked = true;
              }}
            }});
          }}
          return clicked;
        }})()"#
        );
        let res = self.run_js(&js, Stage::Navigate).await?;
        Ok(res.as_deref() == Some("true"))
    }

    /// 从主页进入创建页：可选"视频"菜单 → "发表视频"按钮 → 等表单。
    /// 全程重试：wujie 子应用渲染慢于主页骨架，单次查找必然偶发失败。
    pub async fn enter_create_page(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.try_click_video_menu().await;
            if self
                .sub_text_by_button(self.selectors.publish_entry.as_ref())
                .await
                && self
                    .try_click_button(self.selectors.publish_entry.as_ref())
                    .await
            {
                return self
                    .wait_sub(
                        self.selectors.video_file_input.as_ref(),
                        timeout,
                        Stage::Navigate,
                    )
                    .await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(
                    Code::Timeout,
                    Stage::Navigate,
                    "等待发表视频入口超时（页面未就绪或结构已变化）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn try_click_video_menu(&self) {
        let menu = self.selectors.video_menu.as_ref();
        let js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            const el = sub.querySelector({menu:?});
            if (el && vis(el)) {{
              el.dispatchEvent(new MouseEvent('click', {{bubbles:true, cancelable:true}}));
              return;
            }}
          }}
        }})()"#
        );
        let _ = self.run_js(&js, Stage::Navigate).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn try_click_button(&self, label: &str) -> bool {
        let entry = label;
        let js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const b of sub.querySelectorAll('button')) {{
              if (vis(b) && (b.innerText||'').trim() === {entry:?}) {{
                b.dispatchEvent(new MouseEvent('click', {{bubbles:true, cancelable:true}}));
                return true;
              }}
            }}
          }}
          return false;
        }})()"#
        );
        matches!(
            self.run_js(&js, Stage::Navigate)
                .await
                .ok()
                .flatten()
                .as_deref(),
            Some("true")
        )
    }

    /// 子应用/主文档内是否存在指定文案的可见按钮。
    async fn sub_text_by_button(&self, label: &str) -> bool {
        let js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const b of sub.querySelectorAll('button')) {{
              if (vis(b) && (b.innerText||'').trim() === {label:?}) return true;
            }}
          }}
          return false;
        }})()"#
        );
        match self.page.evaluate(js).await.ok() {
            Some(res) => matches!(res.value(), Some(serde_json::Value::Bool(true))),
            None => false,
        }
    }

    pub async fn upload_video(&self, video_path: &Path) -> Result<()> {
        self.set_file_input(
            self.selectors.video_file_input.as_ref(),
            video_path,
            Stage::Upload,
        )
        .await?;
        self.wait_sub(
            self.selectors.upload_done_indicator.as_ref(),
            STEP_TIMEOUT,
            Stage::Upload,
        )
        .await
    }

    pub async fn fill_title(&self, title: &str) -> Result<()> {
        self.type_into(
            self.selectors.title_input.as_ref(),
            title,
            Stage::Metadata,
            true,
        )
        .await
    }

    pub async fn fill_description(&self, description: &str) -> Result<()> {
        if description.is_empty() {
            return Ok(());
        }
        self.insert_into(
            self.selectors.description_editor.as_ref(),
            description,
            Stage::Metadata,
        )
        .await
    }

    pub async fn fill_tags(&self, tags: &[String]) -> Result<()> {
        if tags.is_empty() {
            return Ok(());
        }
        // 平台话题 = 描述编辑器内联 "#话题 + 空格" 触发自动完成。
        // 先确保编辑器聚焦（描述可能为空，此时光标也应在编辑器里）。
        self.focus_element(self.selectors.description_editor.as_ref(), Stage::Metadata)
            .await?;
        for tag in tags {
            self.insert_text(
                &format!("{} {}", self.selectors.topic_prefix.as_ref(), tag),
                Stage::Metadata,
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Ok(())
    }

    pub async fn set_cover(&self, cover_path: &Path) -> Result<()> {
        self.set_file_input(
            self.selectors.cover_file_input.as_ref(),
            cover_path,
            Stage::Cover,
        )
        .await
    }

    /// 原创声明：默认保守不勾。若已勾选则响亮报错交给人工确认。
    pub async fn check_declaration_conservative(&self) -> Result<()> {
        let label = self.selectors.original_declaration_label.as_ref();
        let js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            for (const lbl of sub.querySelectorAll('.label')) {{
              if ((lbl.innerText||'').trim().includes({label:?})) {{
                const item = lbl.closest('.form-item') || lbl.parentElement;
                if (!item) continue;
                const box = item.querySelector('.ant-checkbox-input');
                if (box) return box.checked ? 'checked' : 'unchecked';
              }}
            }}
          }}
          return 'absent';
        }})()"#
        );
        let state = self.run_js(&js, Stage::Declaration).await?;
        match state.as_deref() {
            Some("checked") => Err(AppError::new(
                Code::PublishRejected,
                Stage::Declaration,
                "页面默认勾选了原创声明；本工具默认保守发布，请人工确认后重试",
            )),
            _ => Ok(()),
        }
    }

    /// 提交并等待平台结果。dry-run 不在此处调用。
    /// 成功判定：页面跳转到 post/list 或 post/manage（平台行为，经真实项目校准）。
    pub async fn submit(&self) -> Result<()> {
        // 1) 等"发表"按钮可用（上传完成前按钮为禁用态）
        let label = self.selectors.submit_button.as_ref();
        let state_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const b of sub.querySelectorAll('button')) {{
              if (vis(b) && (b.innerText||'').trim() === {label:?}) {{
                if (b.disabled || (b.className||'').includes('disabled')) return 'disabled';
                return 'enabled';
              }}
            }}
          }}
          return 'absent';
        }})()"#
        );
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            match self.run_js(&state_js, Stage::Submit).await?.as_deref() {
                Some("enabled") => break,
                Some("absent") => {
                    return Err(AppError::new(
                        Code::SchemaChanged,
                        Stage::Submit,
                        "未找到发表按钮",
                    ));
                }
                _ => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(AppError::new(
                            Code::Timeout,
                            Stage::Submit,
                            "发表按钮一直不可用",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        // 2) 点击
        if !self
            .try_click_button(self.selectors.submit_button.as_ref())
            .await
        {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::Submit,
                "未找到发表按钮",
            ));
        }
        // 3) 等跳转（成功）或平台错误提示（拒绝）
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            let url = self
                .page
                .evaluate("location.href")
                .await
                .ok()
                .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
                .unwrap_or_default();
            if url.contains("post/list") || url.contains("post/manage") {
                return Ok(());
            }
            if self
                .sub_exists(self.selectors.publish_success_indicator.as_ref())
                .await
            {
                return Ok(());
            }
            if let Some(text) = self
                .sub_text(self.selectors.publish_error_indicator.as_ref())
                .await
            {
                return Err(AppError::new(
                    Code::PublishRejected,
                    Stage::Submit,
                    format!("平台拒绝发布: {}", sanitize_message(&text)),
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(
                    Code::Timeout,
                    Stage::Submit,
                    "等待平台发布结果超时（页面未跳转；视频可能已发表，请人工确认后再重试）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    // ── 内部原语 ────────────────────────────────────────────────

    async fn run_js(&self, js: &str, stage: Stage) -> Result<Option<String>> {
        let res = self.page.evaluate(js).await.map_err(|e| {
            AppError::fmt(
                Code::SchemaChanged,
                stage,
                format_args!("页面脚本执行失败: {e}"),
            )
        })?;
        Ok(res.value().and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }))
    }

    async fn wait_sub(&self, selector: &str, timeout: Duration, stage: Stage) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.sub_exists(selector).await {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(Code::Timeout, stage, "等待页面元素超时"));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    async fn sub_exists(&self, selector: &str) -> bool {
        let js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            if (sub.querySelector({selector:?})) return true;
          }}
          return false;
        }})()"#
        );
        match self.page.evaluate(js).await.ok() {
            Some(res) => matches!(res.value(), Some(serde_json::Value::Bool(true))),
            None => false,
        }
    }

    async fn sub_text(&self, selector: &str) -> Option<String> {
        let js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            const el = sub.querySelector({selector:?});
            if (el) return (el.innerText||'').trim().slice(0,120);
          }}
          return '';
        }})()"#
        );
        let res = self.page.evaluate(js).await.ok()?;
        match res.value().and_then(|v| v.as_str().map(String::from)) {
            Some(t) if !t.is_empty() => Some(t),
            _ => None,
        }
    }

    /// DOM.performSearch 全局搜索（穿透 shadow root）拿 nodeId。
    async fn search_node(&self, query: &str, stage: Stage) -> Result<Option<NodeId>> {
        if !self.dom_enabled.get() {
            self.page
                .execute(EnableParams::default())
                .await
                .map_err(|e| {
                    AppError::fmt(
                        Code::SchemaChanged,
                        stage,
                        format_args!("DOM agent 启用失败: {e}"),
                    )
                })?;
            // 锚定文档快照，避免搜索命中已失效节点
            self.page
                .execute(GetDocumentParams::builder().depth(1).build())
                .await
                .map_err(|e| {
                    AppError::fmt(
                        Code::SchemaChanged,
                        stage,
                        format_args!("文档快照失败: {e}"),
                    )
                })?;
            self.dom_enabled.set(true);
        }
        let search = self
            .page
            .execute(PerformSearchParams::new(query))
            .await
            .map_err(|e| {
                AppError::fmt(
                    Code::SchemaChanged,
                    stage,
                    format_args!("搜索上传框失败: {e}"),
                )
            })?;
        if search.result_count == 0 {
            return Ok(None);
        }
        let results = self
            .page
            .execute(GetSearchResultsParams::new(
                search.search_id.clone(),
                0,
                search.result_count,
            ))
            .await
            .map_err(|e| {
                AppError::fmt(
                    Code::SchemaChanged,
                    stage,
                    format_args!("读取搜索结果失败: {e}"),
                )
            })?;
        Ok(results.node_ids.first().cloned())
    }

    async fn set_file_input(&self, query: &str, path: &Path, stage: Stage) -> Result<()> {
        let node = self
            .search_node(query, stage)
            .await?
            .ok_or_else(|| AppError::new(Code::SchemaChanged, stage, "未找到文件上传入口"))?;
        let path_str = path.to_string_lossy().into_owned();
        self.page
            .execute(
                SetFileInputFilesParams::builder()
                    .files(vec![path_str])
                    .node_id(node)
                    .build()
                    .map_err(|e| {
                        AppError::fmt(
                            Code::InternalError,
                            stage,
                            format_args!("构造上传请求失败: {e}"),
                        )
                    })?,
            )
            .await
            .map_err(|e| {
                AppError::fmt(
                    Code::SchemaChanged,
                    stage,
                    format_args!("设置文件失败: {e}"),
                )
            })?;
        Ok(())
    }

    /// 聚焦某元素（JS click）。
    async fn focus_element(&self, selector: &str, stage: Stage) -> Result<()> {
        let js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            const el = sub.querySelector({selector:?});
            if (el) {{ el.focus(); el.click(); return true; }}
          }}
          return false;
        }})()"#
        );
        let ok = self.run_js(&js, stage).await?;
        if ok.as_deref() != Some("true") {
            return Err(AppError::new(Code::SchemaChanged, stage, "未找到输入框"));
        }
        Ok(())
    }

    /// CDP Input.insertText：等价 Playwright keyboard.type 的 Unicode 路径，
    /// React 受控组件与 contenteditable 都按真实输入处理（DOM 赋值会被状态层丢弃）。
    async fn insert_text(&self, text: &str, stage: Stage) -> Result<()> {
        self.page
            .execute(InsertTextParams::new(text))
            .await
            .map_err(|e| {
                AppError::fmt(
                    Code::SchemaChanged,
                    stage,
                    format_args!("文本输入失败: {e}"),
                )
            })?;
        Ok(())
    }

    /// 向元素内插入文本：聚焦 + insertText。
    async fn insert_into(&self, selector: &str, text: &str, stage: Stage) -> Result<()> {
        self.focus_element(selector, stage).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.insert_text(text, stage).await
    }

    /// DOM 赋值 + 事件派发（受控表单标准驱动；非 ASCII 安全）。
    async fn type_into(
        &self,
        selector: &str,
        text: &str,
        stage: Stage,
        is_input: bool,
    ) -> Result<()> {
        let val = serde_json::to_string(text).unwrap_or_default();
        let set_expr = if is_input {
            format!("el.value = {val};")
        } else {
            format!("el.textContent = {val};")
        };
        let js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            const el = sub.querySelector({selector:?});
            if (!el) continue;
            {set_expr}
            el.dispatchEvent(new Event('input', {{bubbles: true}}));
            el.dispatchEvent(new Event('change', {{bubbles: true}}));
            return true;
          }}
          return false;
        }})()"#
        );
        let ok = self.run_js(&js, stage).await?;
        if ok.as_deref() != Some("true") {
            return Err(AppError::new(Code::SchemaChanged, stage, "未找到输入框"));
        }
        Ok(())
    }
}
