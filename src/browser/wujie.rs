//! wujie 微前端感知的页面操作基座。
//!
//! 平台是无界（wujie）微前端：真实 UI 在 `wujie-app` 的 shadowRoot 内。
//! 所有 DOM 操作的查询顺序：逐个 wujie 子应用查找 → 回退主文档，
//! 因此离线 fixture（普通 DOM）与真实平台共用同一套代码。
//!
//! 本基座原语从 publish/page.rs 上抽（2026-09），供 publish 与 videos 共用。

use std::path::Path;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::dom::{
    EnableParams, GetDocumentParams, GetSearchResultsParams, NodeId, PerformSearchParams,
    SetFileInputFilesParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::Page;

use crate::apperr::{AppError, Code, Result, Stage};

/// 子应用定位前缀：找到第一个含 __probe 选择器的 wujie 子应用文档，
/// 找不到回退主文档（离线 fixture 即走此路径）。
/// 全根遍历前缀：__roots 含所有 wujie 子应用文档与主文档。
/// 平台弹窗与入口可能渲染在任意一层（wujie 水合前弹窗在主文档骨架）。
pub const ROOTS: &str = r#"const __roots = [...document.querySelectorAll('wujie-app')]
          .map(a => a.shadowRoot)
          .filter(Boolean);
        __roots.push(document);"#;

pub struct Wujie<'a> {
    page: &'a Page,
    dom_enabled: std::cell::Cell<bool>,
}

impl<'a> Wujie<'a> {
    pub fn new(page: &'a Page) -> Wujie<'a> {
        Wujie {
            page,
            dom_enabled: std::cell::Cell::new(false),
        }
    }

    /// 底层页面句柄（URL 读取、裸 evaluate 等）。
    pub fn page(&self) -> &Page {
        self.page
    }

    pub async fn run_js(&self, js: &str, stage: Stage) -> Result<Option<String>> {
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

    pub async fn wait_sub(&self, selector: &str, timeout: Duration, stage: Stage) -> Result<()> {
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

    pub async fn sub_exists(&self, selector: &str) -> bool {
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

    pub async fn sub_text(&self, selector: &str) -> Option<String> {
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
    pub async fn search_node(&self, query: &str, stage: Stage) -> Result<Option<NodeId>> {
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

    pub async fn set_file_input(&self, query: &str, path: &Path, stage: Stage) -> Result<()> {
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
    pub async fn focus_element(&self, selector: &str, stage: Stage) -> Result<()> {
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
        let ok = self.run_js(js.as_str(), stage).await?;
        if ok.as_deref() != Some("true") {
            return Err(AppError::new(Code::SchemaChanged, stage, "未找到输入框"));
        }
        Ok(())
    }

    /// CDP Input.insertText：等价 Playwright keyboard.type 的 Unicode 路径，
    /// React 受控组件与 contenteditable 都按真实输入处理（DOM 赋值会被状态层丢弃）。
    pub async fn insert_text(&self, text: &str, stage: Stage) -> Result<()> {
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
    pub async fn insert_into(&self, selector: &str, text: &str, stage: Stage) -> Result<()> {
        self.focus_element(selector, stage).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.insert_text(text, stage).await
    }

    /// DOM 赋值 + 事件派发（受控表单标准驱动；非 ASCII 安全）。
    pub async fn type_into(
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
        let ok = self.run_js(js.as_str(), stage).await?;
        if ok.as_deref() != Some("true") {
            return Err(AppError::new(Code::SchemaChanged, stage, "未找到输入框"));
        }
        Ok(())
    }

    /// 在 "x,y"（视口 CSS 像素）坐标处做一次 CDP 真实点击。
    pub async fn real_click_coords(&self, coords: &str, stage: Stage) -> Result<()> {
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

    /// 按一次键（keyDown+keyUp）。
    pub async fn press_key(&self, key: &str, stage: Stage) -> Result<()> {
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

    pub async fn try_click_button(&self, label: &str) -> bool {
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
            self.run_js(js.as_str(), Stage::Navigate)
                .await
                .ok()
                .flatten()
                .as_deref(),
            Some("true")
        )
    }

    /// 子应用/主文档内是否存在指定文案的可见按钮。
    pub async fn sub_text_by_button(&self, label: &str) -> bool {
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
        let coords = self
            .run_js(open_js.as_str(), stage)
            .await?
            .unwrap_or_default();
        if coords.is_empty() {
            return Err(AppError::fmt(
                Code::SchemaChanged,
                stage,
                format_args!("未找到下拉入口：{label}"),
            ));
        }
        self.real_click_coords(&coords, stage).await?;
        tokio::time::sleep(Duration::from_millis(800)).await;
        // 2) 在选项文案本身点击。外层 [class*=item] 可能是整块表单，
        //    其 innerText 包含合集名，点它的中心却不会选择任何合集。
        let opt = serde_json::to_string(option).unwrap_or_default();
        let pick_js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          const center = el => {{
            const r = el.getBoundingClientRect();
            return (r.left + r.width / 2) + ',' + (r.top + r.height / 2);
          }};
          let best = null;
          for (const sub of __roots) {{
            for (const el of sub.querySelectorAll('li, [role=option], [class*=option], [class*=item], div, span')) {{
              if (!vis(el)) continue;
              const t = (el.innerText||'').trim();
              if (t !== {opt} && !t.startsWith({opt} + ' ') && !t.startsWith({opt} + '\n')) continue;
              const r = el.getBoundingClientRect();
              if (r.width <= 0 || r.height <= 0) continue;
              const score = (t === {opt} ? 0 : 1) * 1e9 + r.width * r.height;
              if (!best || score < best.score) best = {{el, score}};
            }}
          }}
          return best ? center(best.el) : '';
        }})()"#
        );
        let picked = self
            .run_js(pick_js.as_str(), stage)
            .await?
            .unwrap_or_default();
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if self.dropdown_has_option(label, option, stage).await? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::fmt(
                    Code::SchemaChanged,
                    stage,
                    format_args!("下拉「{label}」点击「{option}」后未回填到表单，停止提交"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// 只读取表单触发区的值，不把展开菜单里的选项文案误认为已选择。
    pub async fn assert_dropdown_option(
        &self,
        label: &str,
        option: &str,
        stage: Stage,
    ) -> Result<()> {
        if self.dropdown_has_option(label, option, stage).await? {
            Ok(())
        } else {
            Err(AppError::fmt(
                Code::SchemaChanged,
                stage,
                format_args!("下拉「{label}」在提交前不再显示「{option}」，停止提交"),
            ))
        }
    }

    async fn dropdown_has_option(&self, label: &str, option: &str, stage: Stage) -> Result<bool> {
        let lbl = serde_json::to_string(label).unwrap_or_default();
        let opt = serde_json::to_string(option).unwrap_or_default();
        let js = format!(
            r#"(function(){{
          {ROOTS}
          const vis = el => !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
          for (const sub of __roots) {{
            for (const lab of sub.querySelectorAll('.label, div, span')) {{
              if (!vis(lab) || (lab.innerText||'').trim() !== {lbl}) continue;
              const item = lab.closest('.form-item, [class*=form__item], [class*=form-item]') || lab.parentElement;
              if (!item) continue;
              for (const field of item.querySelectorAll('.select-placeholder, [role=combobox], [class*=select__value], [class*=select-value], input')) {{
                if (!vis(field)) continue;
                const value = (field.value || field.innerText || '').trim();
                if (value === {opt} || value.startsWith({opt} + ' ') || value.startsWith({opt} + '\n')) return true;
              }}
            }}
          }}
          return false;
        }})()"#
        );
        Ok(self.run_js(js.as_str(), stage).await?.as_deref() == Some("true"))
    }
}
