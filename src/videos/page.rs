//! 视频管理页对象：list/edit 共用的导航与登录态检测 + edit 的编辑页动作。
//!
//! 与 publish 页同平台（wujie 微前端），底层原语走 browser::wujie 基座。
//! selector 表 2026-09-26 spike 校准（edit3/edit4/edit5 探针，真实账号）：
//! 「修改描述和封面」是路由页 `/platform/post/coverEdit?objectId=<id>`，
//! 描述/短标题为划词编辑（选中→弹层→修改/删除，描述预算 20 字、短标题 16），
//! 封面为「编辑」→ FinderCoverEditor 对话框（file input 上传）。
//! 漂移后由 ~/.sph/patches/videos.json 覆盖。

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use chromiumoxide::Page;

use crate::apperr::{AppError, Code, Result, Stage};
use crate::browser::wujie::{Wujie, ROOTS};
use crate::upstream::sanitize_message;

const STEP_TIMEOUT: Duration = Duration::from_secs(90);
/// 封面预览图生成可能需要数十秒（edit5 实测约 15s）。
const COVER_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// 视频管理页 selector 表（校准自 channels.weixin.qq.com 真实 DOM）。
#[derive(Debug, Clone)]
pub struct Selectors {
    /// 主页就绪标志。
    pub home_ready: Cow<'static, str>,
    /// 登录页标志（存在即未登录 → SESSION_EXPIRED）。
    pub login_indicator: Cow<'static, str>,
    /// 侧边栏"视频"菜单项（进入视频管理页）。
    pub video_menu: Cow<'static, str>,
    /// 编辑页就绪标志（描述/短标题划词容器）。
    pub edit_page_ready: Cow<'static, str>,
    /// 描述划词容器。
    pub edit_desc_area: Cow<'static, str>,
    /// 短标题划词容器。
    pub edit_title_area: Cow<'static, str>,
    /// 划词弹层可见标志（内层 popover 体）。
    pub edit_pop_visible: Cow<'static, str>,
    /// 划词弹层操作项容器（子元素文案 = 修改/删除）。
    pub edit_pop_items: Cow<'static, str>,
    /// 替换文字输入框。
    pub edit_input: Cow<'static, str>,
    /// 输入框旁按钮（文案 = 修改/取消）。
    pub edit_input_buttons: Cow<'static, str>,
    /// 已修改字数徽标（形如"已修改描述 4/20"）。
    pub edit_edited_badge: Cow<'static, str>,
    /// 页面级"完成"按钮文案。
    pub edit_done_label: Cow<'static, str>,
    /// 确认弹窗（weui 对话框）。
    pub edit_confirm_dialog: Cow<'static, str>,
    /// 确认弹窗提交按钮文案。
    pub edit_confirm_label: Cow<'static, str>,
    /// 封面"编辑"按钮（第一个 = 个人主页卡片 3:4）。
    pub edit_cover_button: Cow<'static, str>,
    /// 封面编辑器可见标志。
    pub edit_cover_dialog: Cow<'static, str>,
    /// 封面文件框。
    pub edit_cover_file_input: Cow<'static, str>,
    /// 封面编辑器确认按钮文案。
    pub edit_cover_confirm_label: Cow<'static, str>,
    /// 编辑成功标志（top-tip 出现且含"修改成功"，或路由离开编辑页）。
    pub edit_success_indicator: Cow<'static, str>,
    /// 平台错误/拒绝提示。
    pub edit_error_indicator: Cow<'static, str>,
}

/// 内置 selector（2026-09-26 真实平台校准值）。
pub static DEFAULT_SELECTORS: Selectors = Selectors {
    home_ready: Cow::Borrowed(".brand-name"),
    login_indicator: Cow::Borrowed(".qrcode-tip, #qrcode-login, .login-qrcode"),
    video_menu: Cow::Borrowed(".finder-ui-desktop-menu__sub__li"),
    edit_page_ready: Cow::Borrowed(".edit-select-area"),
    edit_desc_area: Cow::Borrowed(".edit-desc-content"),
    edit_title_area: Cow::Borrowed(".edit-shorttitle-content"),
    edit_pop_visible: Cow::Borrowed(".edit-pop .weui-desktop-popover"),
    edit_pop_items: Cow::Borrowed(".edit-pop-content div"),
    edit_input: Cow::Borrowed(".input-item-input"),
    edit_input_buttons: Cow::Borrowed(".edit-input-btns button"),
    edit_edited_badge: Cow::Borrowed(".edited-count"),
    edit_done_label: Cow::Borrowed("完成"),
    edit_confirm_dialog: Cow::Borrowed(".weui-desktop-dialog__wrp"),
    edit_confirm_label: Cow::Borrowed("确认修改"),
    edit_cover_button: Cow::Borrowed(".edit-btn"),
    edit_cover_dialog: Cow::Borrowed(".cover-set-wrap"),
    edit_cover_file_input: Cow::Borrowed("input[type=file][accept*='image']"),
    edit_cover_confirm_label: Cow::Borrowed("确认"),
    edit_success_indicator: Cow::Borrowed(".weui-desktop-tips, .weui-desktop-toast"),
    edit_error_indicator: Cow::Borrowed(".weui-desktop-tips, .weui-desktop-toast"),
};

pub struct VideoManagePage<'a> {
    page: &'a Page,
    pub selectors: &'a Selectors,
    pub wujie: Wujie<'a>,
}

impl<'a> VideoManagePage<'a> {
    pub fn new(page: &'a Page, selectors: &'a Selectors) -> VideoManagePage<'a> {
        VideoManagePage {
            page,
            selectors,
            wujie: Wujie::new(page),
        }
    }

    async fn current_url(&self) -> String {
        self.page
            .evaluate("location.href")
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_str().map(String::from)))
            .unwrap_or_default()
    }

    /// 未登录检测：URL 落在 login/passport 或登录页标志出现。
    pub async fn is_login_page(&self) -> bool {
        let url = self.current_url().await;
        if url.contains("login") || url.contains("passport") {
            return true;
        }
        self.page
            .find_element(self.selectors.login_indicator.as_ref())
            .await
            .is_ok()
    }

    /// 校验当前仍处于已登录态。
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

    /// 主页就绪检测（每轮复查登录页，原因同发布页）。
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
        let res = self.wujie.run_js(&js, Stage::Navigate).await?;
        Ok(res.as_deref() == Some("true"))
    }

    /// 直达编辑页（coverEdit 路由）。headless 下整页加载深层路由会被弹回
    /// /platform（2026-09-26 edit6 探针实测），必须用 SPA 内部跳转：
    /// history.pushState + popstate，wujie 路由同步会把子应用切到编辑页。
    /// `skip_navigate`：测试注入 navigate_url 时页面已在目标页，只等就绪。
    pub async fn open_edit_page(&self, object_id: &str, skip_navigate: bool) -> Result<()> {
        if !skip_navigate {
            let js = format!(
                r#"(function(){{
              history.pushState({{}}, '', '/platform/post/coverEdit?objectId={}');
              window.dispatchEvent(new PopStateEvent('popstate'));
              return location.href;
            }})()"#,
                object_id.replace('/', "%2F")
            );
            self.wujie.run_js(&js, Stage::Navigate).await?;
        }
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            self.require_logged_in().await?;
            if self
                .wujie
                .sub_exists(self.selectors.edit_page_ready.as_ref())
                .await
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::fmt(
                    Code::Timeout,
                    Stage::Navigate,
                    format_args!(
                        "等待编辑页就绪超时（当前 URL: {}）",
                        self.current_url().await
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// 找指定容器内文案匹配的可见元素中心坐标（"x,y"）。
    async fn pos_of(&self, scope: &str, text: &str, stage: Stage) -> Result<Option<String>> {
        let js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll({scope:?})) {{
              if (!vis(el)) continue;
              if ((el.innerText || '').trim() !== {text:?}) continue;
              el.scrollIntoView({{block: 'center', inline: 'center'}});
              const b = el.getBoundingClientRect();
              return (b.x + b.width/2) + ',' + (b.y + b.height/2);
            }}
          }}
          return '';
        }})()"#
        );
        let out = self.wujie.run_js(&js, stage).await?.unwrap_or_default();
        Ok(if out.is_empty() { None } else { Some(out) })
    }

    /// 轮询直到 JS 表达式返回 'y'。
    async fn poll_js(&self, js: &str, timeout: Duration, stage: Stage, what: &str) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.require_logged_in().await?;
            if self.wujie.run_js(js, stage).await?.as_deref() == Some("y") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::fmt(
                    Code::Timeout,
                    stage,
                    format_args!("等待{what}超时（页面结构可能已变化）"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    /// 可见性判断 JS（shadow roots 全遍历）。
    fn visible_js(selector: &str) -> String {
        format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll({selector:?})) {{
              if (vis(el)) return 'y';
            }}
          }}
          return '';
        }})()"#
        )
    }

    /// 划词替换：把 `area` 容器里 [start, start+old_len) 这段文字换成 new_text
    ///（new_text 为空 = 删除）。全部走平台原生的划词编辑链路：
    /// 选中 → selectionEnd → 弹层「修改/删除」→ 输入框 → 提交编辑动作。
    pub async fn apply_span_edit(
        &self,
        area: &str,
        expected_full: &str,
        start: usize,
        old_len: usize,
        new_text: &str,
    ) -> Result<()> {
        // 1) 选中目标区间并触发 selectionEnd（bundle：容器监听该事件开弹层）
        let select_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll({area:?})) {{
              if (!vis(el)) continue;
              const tn = [...el.childNodes].find(n => n.nodeType === 3 && n.textContent.trim().length > 0);
              if (!tn) return 'no-text-node';
              if (tn.textContent !== {expected_full:?}) return 'content-mismatch';
              const range = document.createRange();
              range.setStart(tn, {start});
              range.setEnd(tn, {end});
              const s = window.getSelection(); s.removeAllRanges(); s.addRange(range);
              el.dispatchEvent(new Event('selectionEnd'));
              if (el.parentElement) el.parentElement.dispatchEvent(new Event('selectionEnd'));
              return 'y';
            }}
          }}
          return 'no-area';
        }})()"#,
            start = start,
            end = start + old_len,
        );
        let out = self
            .wujie
            .run_js(&select_js, Stage::Metadata)
            .await?
            .unwrap_or_default();
        match out.as_str() {
            "y" => {}
            "content-mismatch" => {
                return Err(AppError::new(
                    Code::SchemaChanged,
                    Stage::Metadata,
                    "编辑页显示的当前文本与列表读取结果不一致（可能被其他会话改动），请重新执行 sph list 确认后再改",
                ))
            }
            other => {
                return Err(AppError::fmt(
                    Code::SchemaChanged,
                    Stage::Metadata,
                    format_args!("未找到可编辑文本容器（{other}）"),
                ))
            }
        }

        // 2) 等划词弹层可见
        let pop_js = Self::visible_js(self.selectors.edit_pop_visible.as_ref());
        self.poll_js(
            &pop_js,
            Duration::from_secs(10),
            Stage::Metadata,
            "划词弹层",
        )
        .await?;

        // 3) 点「修改」或「删除」
        let label = if new_text.is_empty() {
            "删除"
        } else {
            "修改"
        };
        let pos = self
            .pos_of(
                self.selectors.edit_pop_items.as_ref(),
                label,
                Stage::Metadata,
            )
            .await?
            .ok_or_else(|| {
                AppError::fmt(
                    Code::SchemaChanged,
                    Stage::Metadata,
                    format_args!("划词弹层里未找到「{label}」操作"),
                )
            })?;
        self.wujie.real_click_coords(&pos, Stage::Metadata).await?;

        if !new_text.is_empty() {
            // 4) 等输入框 → 填替换文字（handleDescInput 直接读 DOM value）
            let input_js = Self::visible_js(self.selectors.edit_input.as_ref());
            self.poll_js(
                &input_js,
                Duration::from_secs(10),
                Stage::Metadata,
                "替换输入框",
            )
            .await?;
            let fill_js = format!(
                r#"(function(){{
              {ROOTS}
              const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
              for (const sub of __roots) {{
                // 页面上描述/短标题各有一个输入框（一个隐藏一个可见），必须填可见的那个
                for (const inp of sub.querySelectorAll({input:?})) {{
                  if (!vis(inp)) continue;
                  inp.value = {new_text:?};
                  inp.dispatchEvent(new Event('input', {{bubbles: true}}));
                  return 'y';
                }}
              }}
              return '';
            }})()"#,
                input = self.selectors.edit_input.as_ref(),
            );
            if self
                .wujie
                .run_js(&fill_js, Stage::Metadata)
                .await?
                .as_deref()
                != Some("y")
            {
                return Err(AppError::new(
                    Code::SchemaChanged,
                    Stage::Metadata,
                    "未找到替换文字输入框",
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            // 5) 点输入框旁「修改」提交这条编辑动作
            let pos = self
                .pos_of(
                    self.selectors.edit_input_buttons.as_ref(),
                    "修改",
                    Stage::Metadata,
                )
                .await?
                .ok_or_else(|| {
                    AppError::new(
                        Code::SchemaChanged,
                        Stage::Metadata,
                        "未找到输入框的「修改」按钮",
                    )
                })?;
            self.wujie.real_click_coords(&pos, Stage::Metadata).await?;
        }

        // 6) 校验编辑动作已计入（徽标出现）+ 清掉残留选区
        let badge_js = Self::visible_js(self.selectors.edit_edited_badge.as_ref());
        if let Err(e) = self
            .poll_js(
                &badge_js,
                Duration::from_secs(10),
                Stage::Metadata,
                "已修改字数徽标",
            )
            .await
        {
            // 诊断：dump 当前编辑页关键状态（排查 headless 差异）
            let dump_js = format!(
                r#"(function(){{
              {ROOTS}
              const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
              const out = [];
              for (const sub of __roots) {{
                for (const el of sub.querySelectorAll('.edited-count, .edit-select-area .edited, .weui-desktop-tips, .edit-pop .weui-desktop-popover, .input-item-input')) {{
                  out.push((typeof el.className === 'string' ? el.className.slice(0,45) : el.tagName)
                    + ' vis=' + vis(el) + ' text=' + (el.innerText || '').trim().slice(0, 50));
                }}
              }}
              return out.join(' | ') || '(空)';
            }})()"#
            );
            let dump = self
                .wujie
                .run_js(&dump_js, Stage::Metadata)
                .await?
                .unwrap_or_default();
            return Err(AppError::fmt(
                e.code,
                e.stage,
                format_args!("{}（现场: {dump}）", e.message),
            ));
        }
        let _ = self
            .wujie
            .run_js(
                "window.getSelection() && window.getSelection().removeAllRanges(), 'y'",
                Stage::Metadata,
            )
            .await;
        Ok(())
    }

    /// 改封面：等预览图就绪 → 编辑相应比例 → 设置文件 → 确认。
    pub async fn edit_set_cover(&self, cover_path: &Path, landscape: bool) -> Result<()> {
        // 预览图生成中点不开编辑器（edit5 实测），等到「生成中」提示消失
        let ready_js = r#"(function(){
          const roots = [...document.querySelectorAll('wujie-app')].map(a => a.shadowRoot).filter(Boolean);
          roots.push(document);
          for (const r of roots) {
            for (const el of r.querySelectorAll('.img-popover-wrap .weui-desktop-popover__desc, .vertical-cover-wrap .loading-wrap, .horizon-cover-wrap .loading-wrap')) {
              if ((el.innerText || '').includes('生成中') || (el.classList.contains('loading-wrap') && (el.offsetWidth || el.offsetHeight || el.getClientRects().length))) return '';
            }
          }
          return 'y';
        })()"#;
        self.poll_js(
            ready_js,
            COVER_READY_TIMEOUT,
            Stage::Cover,
            "封面预览图生成",
        )
        .await?;

        let scoped_button = if landscape {
            ".horizon-cover-wrap .edit-btn"
        } else {
            ".vertical-cover-wrap .edit-btn"
        };
        let pos = self
            .pos_of(scoped_button, "编辑", Stage::Cover)
            .await?
            .ok_or_else(|| {
                AppError::new(Code::SchemaChanged, Stage::Cover, "未找到封面「编辑」按钮")
            })?;
        self.wujie.real_click_coords(&pos, Stage::Cover).await?;

        if landscape {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let pos = self
                .pos_of(
                    ".ant-popover .btn-directly-edit button",
                    "直接编辑",
                    Stage::Cover,
                )
                .await?
                .ok_or_else(|| {
                    AppError::new(
                        Code::SchemaChanged,
                        Stage::Cover,
                        "横版封面推荐浮层未找到「直接编辑」",
                    )
                })?;
            self.wujie.real_click_coords(&pos, Stage::Cover).await?;
        }
        let dlg_js = Self::visible_js(self.selectors.edit_cover_dialog.as_ref());
        self.poll_js(&dlg_js, Duration::from_secs(15), Stage::Cover, "封面编辑器")
            .await?;

        self.wujie
            .set_file_input(
                self.selectors.edit_cover_file_input.as_ref(),
                cover_path,
                Stage::Cover,
            )
            .await?;

        // 上传处理（裁剪步骤渲染）给足时间再点确认
        tokio::time::sleep(Duration::from_secs(3)).await;
        let pos = self
            .pos_of(
                &format!("{} button", self.selectors.edit_confirm_dialog.as_ref()),
                self.selectors.edit_cover_confirm_label.as_ref(),
                Stage::Cover,
            )
            .await?
            .ok_or_else(|| {
                AppError::new(
                    Code::SchemaChanged,
                    Stage::Cover,
                    "封面编辑器里未找到「确认」按钮",
                )
            })?;
        self.wujie.real_click_coords(&pos, Stage::Cover).await?;

        // 等编辑器关闭（回到编辑页）
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if self.wujie.run_js(&dlg_js, Stage::Cover).await?.as_deref() != Some("y") {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(
                    Code::Timeout,
                    Stage::Cover,
                    "封面编辑器确认后未关闭（结构可能已变化）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    /// 提交修改：页面「完成」→ 确认弹窗「确认修改」→ 成功标志 / 平台错误。
    pub async fn submit_edit(&self) -> Result<()> {
        // 1) 页面级「完成」（不在对话框内）
        let pos = self
            .pos_of(
                "button",
                self.selectors.edit_done_label.as_ref(),
                Stage::Submit,
            )
            .await?
            .ok_or_else(|| {
                AppError::new(Code::SchemaChanged, Stage::Submit, "未找到「完成」按钮")
            })?;
        self.wujie.real_click_coords(&pos, Stage::Submit).await?;

        // 2) 确认弹窗
        let dlg_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const w of sub.querySelectorAll({dlg:?})) {{
              if (!vis(w)) continue;
              for (const b of w.querySelectorAll('button')) {{
                if ((b.innerText || '').trim() === {label:?}) return 'y';
              }}
            }}
          }}
          return '';
        }})()"#,
            dlg = self.selectors.edit_confirm_dialog.as_ref(),
            label = self.selectors.edit_confirm_label.as_ref(),
        );
        self.poll_js(&dlg_js, Duration::from_secs(15), Stage::Submit, "确认弹窗")
            .await?;
        let pos = self
            .pos_of(
                &format!("{} button", self.selectors.edit_confirm_dialog.as_ref()),
                self.selectors.edit_confirm_label.as_ref(),
                Stage::Submit,
            )
            .await?
            .ok_or_else(|| {
                AppError::new(
                    Code::SchemaChanged,
                    Stage::Submit,
                    "确认弹窗里未找到提交按钮",
                )
            })?;
        self.wujie.real_click_coords(&pos, Stage::Submit).await?;

        // 3) 结果：top-tip「修改成功」或路由离开编辑页 = 成功；错误 tip = 平台拒绝。
        // tips 可能含无关内容，须逐条看文案。
        let tips_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const out = [];
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll({sel:?})) {{
              if (vis(el)) out.push((el.innerText || '').trim());
            }}
          }}
          return out.join('\n');
        }})()"#,
            sel = format!(
                "{}, {}",
                self.selectors.edit_success_indicator.as_ref(),
                self.selectors.edit_error_indicator.as_ref()
            ),
        );
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            let tips = self
                .wujie
                .run_js(&tips_js, Stage::Submit)
                .await?
                .unwrap_or_default();
            if tips.lines().any(|t| t.contains("修改成功")) {
                return Ok(());
            }
            if let Some(bad) = tips
                .lines()
                .find(|t| t.contains("修改失败") || t.contains("错误码") || t.contains("拒绝"))
            {
                return Err(AppError::new(
                    Code::PublishRejected,
                    Stage::Submit,
                    format!("平台拒绝修改: {}", sanitize_message(bad)),
                ));
            }
            if !self.current_url().await.contains("coverEdit") {
                // handleQuit 跳回列表页（成功后 2s）
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(
                    Code::Timeout,
                    Stage::Submit,
                    "等待修改结果超时（修改可能已生效，请用 sph list 人工确认）",
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
