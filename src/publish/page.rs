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
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchMouseEventParams, DispatchMouseEventType, InsertTextParams, MouseButton,
};
use chromiumoxide::Page;

use crate::apperr::{AppError, Code, Result, Stage};
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
    /// 定时 radio 的文案（"定时"，精确匹配，不是"不定时"）。
    pub schedule_radio_label: Cow<'static, str>,
    /// 定时日期输入框（placeholder 线索）。
    pub schedule_date_input: Cow<'static, str>,
    /// 日期 picker 头部（当前月份）。
    pub schedule_picker_header: Cow<'static, str>,
    /// 日期 picker 下月箭头。
    pub schedule_picker_next: Cow<'static, str>,
    /// 日期 picker 日期单元。
    pub schedule_picker_day: Cow<'static, str>,
    /// 定时时间输入框（placeholder 线索）。
    pub schedule_time_input: Cow<'static, str>,
    /// "添加到合集"文案。
    pub collection_label: Cow<'static, str>,
    /// "链接"文案。
    pub link_label: Cow<'static, str>,
    /// "活动"文案。
    pub activity_label: Cow<'static, str>,
    /// "视频标注"文案。
    pub mark_label: Cow<'static, str>,
    /// 视频标注选项关键词（"含AI"）。
    pub mark_ai_keyword: Cow<'static, str>,
    /// 视频标注选项元素。
    pub mark_option: Cow<'static, str>,
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
    schedule_radio_label: Cow::Borrowed("定时"),
    schedule_date_input: Cow::Borrowed("input[placeholder='请选择发表时间']"),
    schedule_picker_header: Cow::Borrowed(".weui-desktop-picker__panel__hd"),
    schedule_picker_next: Cow::Borrowed(
        ".weui-desktop-picker__panel__hd .weui-desktop-btn__icon__right",
    ),
    schedule_picker_day: Cow::Borrowed(".weui-desktop-picker__table a"),
    schedule_time_input: Cow::Borrowed("input[placeholder='请选择时间']"),
    collection_label: Cow::Borrowed("添加到合集"),
    link_label: Cow::Borrowed("链接"),
    activity_label: Cow::Borrowed("活动"),
    mark_label: Cow::Borrowed("视频标注"),
    mark_ai_keyword: Cow::Borrowed("含AI"),
    mark_option: Cow::Borrowed(".mark-tag-option, .option-main"),
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

    /// 未登录检测：URL 落在 login/passport（最稳，平台改版不依赖 DOM）或登录页标志出现。
    pub async fn is_login_page(&self) -> bool {
        if let Ok(r) = self.page.evaluate("location.href").await {
            let url = r
                .value()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            if url.contains("login") || url.contains("passport") {
                return true;
            }
        }
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
    ///
    /// 未登录时平台是 JS 异步跳转 login.html，navigate 后立刻查 URL 可能还没跳，
    /// 所以等待循环里每轮都复查登录页——否则会话失效会表现为等 `.brand-name` 超时。
    pub async fn wait_home_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.require_logged_in().await?;
            if self
                .page
                .find_element(self.selectors.home_ready.as_ref())
                .await
                .is_ok()
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(
                    Code::Timeout,
                    Stage::Navigate,
                    "等待主页就绪超时（页面未就绪或结构已变化）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
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
            self.require_logged_in().await?;
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

    /// 通用下拉选择：点 label 行的下拉区 → 弹层里点选项文案 → 回填校验。
    ///
    /// 点击必须走 CDP 真实鼠标事件：antd/weui 的 Select 监听 mousedown 序列，
    /// JS 合成 dispatchEvent('click') 不会展开弹层（2026-09 合集失败实测）。
    pub async fn select_dropdown_option(
        &self,
        label: &str,
        option: &str,
        stage: Stage,
    ) -> Result<()> {
        let lbl = serde_json::to_string(label).unwrap_or_default();
        // 1) 找下拉触发区，返回中心点视口坐标（点击由 CDP 完成，不在 JS 里点）
        let open_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const center = el => {{
            const r = el.getBoundingClientRect();
            return (r.left + r.width / 2) + ',' + (r.top + r.height / 2);
          }};
          for (const sub of __roots) {{
            for (const lab of sub.querySelectorAll('.label, div, span')) {{
              if (!vis(lab) || (lab.innerText||'').trim() !== {lbl}) continue;
              const item = lab.closest('.form-item, [class*=form__item], [class*=form-item]') || lab.parentElement;
              if (!item) continue;
              const ph = item.querySelector('.select-placeholder, [class*=select], [class*=dropdown]');
              if (ph && vis(ph)) {{ return center(ph); }}
              if (vis(item)) {{ return center(item); }}
            }}
          }}
          return '';
        }})()"#
        );
        let coords = self.run_js(&open_js, stage).await?.unwrap_or_default();
        if coords.is_empty() {
            return Err(AppError::fmt(
                Code::SchemaChanged,
                stage,
                format_args!("未找到下拉入口：{label}"),
            ));
        }
        self.real_click_coords(&coords, stage).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        // 2) 弹层里找选项（精确文案优先，其次包含），同样取坐标后真实点击
        let opt = serde_json::to_string(option).unwrap_or_default();
        let pick_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const center = el => {{
            const r = el.getBoundingClientRect();
            return (r.left + r.width / 2) + ',' + (r.top + r.height / 2);
          }};
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll('li, [class*=option], [class*=item], .weui-desktop-select__option')) {{
              if (!vis(el)) continue;
              if ((el.innerText||'').trim() === {opt}) {{ return center(el); }}
            }}
          }}
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll('li, [class*=option], [class*=item]')) {{
              if (!vis(el)) continue;
              if ((el.innerText||'').trim().includes({opt})) {{ return center(el); }}
            }}
          }}
          return '';
        }})()"#
        );
        let picked = self.run_js(&pick_js, stage).await?.unwrap_or_default();
        if picked.is_empty() {
            return Err(AppError::fmt(
                Code::SchemaChanged,
                stage,
                format_args!(
                    "下拉「{label}」中没有选项「{option}」（账号可能没有可用的合集/链接/活动）"
                ),
            ));
        }
        self.real_click_coords(&picked, stage).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }

    /// 在 "x,y"（视口 CSS 像素）坐标处做一次 CDP 真实点击。
    async fn real_click_coords(&self, coords: &str, stage: Stage) -> Result<()> {
        let (x, y): (f64, f64) = coords
            .split_once(',')
            .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
            .ok_or_else(|| AppError::new(Code::InternalError, stage, "点击坐标解析失败"))?;
        for ty in [
            DispatchMouseEventType::MousePressed,
            DispatchMouseEventType::MouseReleased,
        ] {
            self.page
                .execute(
                    DispatchMouseEventParams::builder()
                        .r#type(ty)
                        .x(x)
                        .y(y)
                        .button(MouseButton::Left)
                        .click_count(1)
                        .build()
                        .map_err(|e| {
                            AppError::fmt(
                                Code::InternalError,
                                stage,
                                format_args!("构造点击事件失败: {e}"),
                            )
                        })?,
                )
                .await
                .map_err(|e| {
                    AppError::fmt(
                        Code::SchemaChanged,
                        stage,
                        format_args!("真实点击失败: {e}"),
                    )
                })?;
        }
        Ok(())
    }

    /// 勾选"含 AI 生成内容"视频标注。
    ///
    /// 真实控件是 Vue 选项：点一次选中，再点一次取消（`selectOption` 按 tagType 切换）。
    /// 使用一次 CDP 鼠标点击；合成 click 不一定触发平台的 mousedown 监听。
    pub async fn mark_ai_content(&self) -> Result<()> {
        let kw = self.selectors.mark_ai_keyword.as_ref();
        let kw_json = serde_json::to_string(kw).unwrap_or_default();
        let mark_sel = self.selectors.mark_option.as_ref();
        // 1) 展开折叠区：点"视频标注"占位
        let lbl = serde_json::to_string(self.selectors.mark_label.as_ref()).unwrap_or_default();
        let expand_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll('.select-placeholder')) {{
              if (vis(el) && (el.innerText||'').includes({lbl})) {{
                el.dispatchEvent(new MouseEvent('click', {{bubbles:true, cancelable:true}}));
                return true;
              }}
            }}
          }}
          return false;
        }})()"#
        );
        let expanded = self.run_js(&expand_js, Stage::Declaration).await?;
        if expanded.as_deref() != Some("true") {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::Declaration,
                "未找到视频标注入口",
            ));
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        // 2) 探测当前状态
        let probe_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll('.select-placeholder')) {{
              if (vis(el) && (el.innerText||'').includes({kw_json})) return 'checked';
            }}
          }}
          let found = false;
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll({mark_sel:?})) {{
              const t = (el.innerText||'').trim();
              if (t.includes({kw_json})) {{
                found = true;
                const cls = (typeof el.className === 'string' ? el.className : '');
                if (cls.includes('checked') || cls.includes('is-selected') || el.getAttribute('aria-checked') === 'true') return 'checked';
              }}
            }}
          }}
          return found ? 'unchecked' : 'absent';
        }})()"#,
            mark_sel = mark_sel
        );
        let state = self.run_js(&probe_js, Stage::Declaration).await?;
        if state.as_deref() == Some("checked") {
            return Ok(());
        }
        // 3) 只点一次。选项在 .mark-tag-option 上 stopPropagation 后 selectOption。
        let click_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll({mark_sel:?})) {{
              if (!vis(el)) continue;
              const t = (el.innerText||'').trim();
              if (!t.includes({kw_json})) continue;
              if (!el.classList.contains('mark-tag-option')) continue;
              el.scrollIntoView({{block:'center'}});
              const r = el.getBoundingClientRect();
              return (r.left + r.width / 2) + ',' + (r.top + r.height / 2);
            }}
          }}
          return '';
        }})()"#,
            mark_sel = mark_sel
        );
        let clicked = self
            .run_js(&click_js, Stage::Declaration)
            .await?
            .unwrap_or_default();
        if clicked.is_empty() {
            return Err(AppError::fmt(
                Code::SchemaChanged,
                Stage::Declaration,
                format_args!("未找到视频标注选项（关键词 {kw}）"),
            ));
        }
        self.real_click_coords(&clicked, Stage::Declaration).await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // 4) 校验
        let state2 = self.run_js(&probe_js, Stage::Declaration).await?;
        if state2.as_deref() != Some("checked") {
            return Err(AppError::new(
                Code::PublishRejected,
                Stage::Declaration,
                format!(
                    "视频标注勾选后状态校验未通过（当前状态：{}）",
                    state2.unwrap_or_default()
                ),
            ));
        }
        Ok(())
    }

    /// 设置定时发表：radio"定时" → 日期 picker → 时间输入（键盘级事件，React 18 受控）。
    /// 交互序列校准自 frankwei2019/auto-weixin-video 的踩坑记录。
    pub async fn set_schedule(&self, at: time::OffsetDateTime) -> Result<()> {
        // 1) 切"定时"radio（点文案 span；React 受控需真实事件）
        let label = self.selectors.schedule_radio_label.as_ref();
        let radio_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const sp of sub.querySelectorAll('span.weui-desktop-form__check-content')) {{
              if (vis(sp) && (sp.innerText||'').trim() === {label:?}) {{
                sp.dispatchEvent(new MouseEvent('click', {{bubbles:true, cancelable:true}}));
                return true;
              }}
            }}
          }}
          return false;
        }})()"#
        );
        let clicked = self.run_js(&radio_js, Stage::Schedule).await?;
        if clicked.as_deref() != Some("true") {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::Schedule,
                "未找到定时选项",
            ));
        }
        // 等 React 重渲染出日期/时间输入
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // 2) 日期：触发 picker → 必要时翻月 → 点目标日
        let date_input = self.selectors.schedule_date_input.as_ref();
        let focus_js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            const inp = sub.querySelector({date_input:?});
            if (inp) {{ inp.focus(); inp.click(); return true; }}
          }}
          return false;
        }})()"#
        );
        let has_picker = self.run_js(&focus_js, Stage::Schedule).await?;
        if has_picker.as_deref() == Some("true") {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            // 翻月：读头部月份，差几个月点几次右箭头
            let header = self.selectors.schedule_picker_header.as_ref();
            let month_js = format!(
                r#"(function(){{
              {ROOTS}
              for (const sub of __roots) {{
                for (const h of sub.querySelectorAll({header:?})) {{
                  const t = (h.innerText||'').trim();
                  const m = t.match(/(\d+)月/);
                  if (m) return m[1];
                }}
              }}
              return '';
            }})()"#
            );
            let cur_month = self
                .run_js(&month_js, Stage::Schedule)
                .await?
                .and_then(|m| m.parse::<u8>().ok());
            let target_month = at.month() as u8;
            if let Some(cur) = cur_month {
                if cur != target_month {
                    let clicks = (target_month as i32 - cur as i32).rem_euclid(12) as usize;
                    let arrow = self.selectors.schedule_picker_next.as_ref();
                    for _ in 0..clicks.clamp(1, 12) {
                        let click_js = format!(
                            r#"(function(){{
                          {ROOTS}
                          for (const sub of __roots) {{
                            const a = sub.querySelector({arrow:?});
                            if (a) {{ a.dispatchEvent(new MouseEvent('click', {{bubbles:true}})); return true; }}
                          }}
                          return false;
                        }})()"#
                        );
                        let _ = self.run_js(&click_js, Stage::Schedule).await;
                        tokio::time::sleep(Duration::from_millis(600)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            // 点目标日（非 disabled）
            let day = at.day();
            let day_sel = self.selectors.schedule_picker_day.as_ref();
            let day_js = format!(
                r#"(function(){{
              {ROOTS}
              for (const sub of __roots) {{
                for (const a of sub.querySelectorAll({day_sel:?})) {{
                  if ((a.innerText||'').trim() === {day:?} && !(a.className||'').includes('disabled')) {{
                    a.dispatchEvent(new MouseEvent('click', {{bubbles:true, cancelable:true}}));
                    return true;
                  }}
                }}
              }}
              return false;
            }})()"#,
                day = day.to_string()
            );
            let day_clicked = self.run_js(&day_js, Stage::Schedule).await?;
            if day_clicked.as_deref() != Some("true") {
                return Err(AppError::fmt(
                    Code::ScheduleInvalid,
                    Stage::Schedule,
                    format_args!(
                        "日期选择失败：{} 日不可选（可能早于平台允许的最小日期）",
                        day
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(800)).await;
        }

        // 3) 时间：聚焦 + 全选 + insertText 替换选区 + Tab blur（React 18 onChange 同步）
        let time_sel = self.selectors.schedule_time_input.as_ref();
        let time_str = format!("{:02}:{:02}", at.hour(), at.minute());
        let prep_js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            const inp = sub.querySelector({time_sel:?});
            if (inp) {{ inp.focus(); inp.click(); inp.select(); return true; }}
          }}
          return false;
        }})()"#
        );
        let focused = self.run_js(&prep_js, Stage::Schedule).await?;
        if focused.as_deref() != Some("true") {
            return Err(AppError::new(
                Code::SchemaChanged,
                Stage::Schedule,
                "未找到定时时间输入框",
            ));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.insert_text(&time_str, Stage::Schedule).await?;
        // Tab blur 触发 onChange 同步
        self.press_key("Tab", Stage::Schedule).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 4) 校验回填值
        let verify_js = format!(
            r#"(function(){{
          {ROOTS}
          for (const sub of __roots) {{
            const inp = sub.querySelector({time_sel:?});
            if (inp) return inp.value || '';
          }}
          return '';
        }})()"#
        );
        let actual = self
            .run_js(&verify_js, Stage::Schedule)
            .await?
            .unwrap_or_default();
        if actual.trim() != time_str {
            return Err(AppError::fmt(
                Code::ScheduleInvalid,
                Stage::Schedule,
                format_args!("时间回填校验失败：期望 {time_str}，实际 {actual}"),
            ));
        }
        Ok(())
    }

    /// 按一次键（keyDown+keyUp）。
    async fn press_key(&self, key: &str, stage: Stage) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchKeyEventParams, DispatchKeyEventType,
        };
        for t in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            self.page
                .execute(
                    DispatchKeyEventParams::builder()
                        .r#type(t)
                        .key(key)
                        .build()
                        .map_err(|e| {
                            AppError::fmt(
                                Code::SchemaChanged,
                                stage,
                                format_args!("构造按键失败: {e}"),
                            )
                        })?,
                )
                .await
                .map_err(|e| {
                    AppError::fmt(Code::SchemaChanged, stage, format_args!("按键失败: {e}"))
                })?;
        }
        Ok(())
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
