//! 前端全局函数调用封装（Rust → Webview JS，遵循 SOLID）
//!
//! 入口：`FrontendService::call`；出口：前端 `window.<fn_name>(...args)`。
//!
//! 关键点：Tauri 2.x 的 `WebviewWindow::eval` 为同步「发射即忘」，无法直接获取 JS 返回值，
//! 因此采用 **eval + 一次性事件回调** 模式：
//!   1. 生成唯一响应事件名，注册一次性监听
//!   2. eval 脚本**同步调用** `window.<fn>`（要求前端方法为同步函数，不支持 async），
//!      把返回值（或异常）经 `window.__TAURI__.event.emit` 回传
//! 参数经 `serde_json` 序列化后内联进脚本，天然规避反引号/换行/引号逃逸注入。
//!
//! # 声明式调用（OpenFeign 风格，调用方无感知）
//!
//! 用 [`frontend_api!`] 宏一次声明接口清单，宏在编译期展开为完整方法实现：
//! 方法参数自动 JSON 序列化传入，返回值自动反序列化，调用方无需关心任何调用细节。
//!
//! ```rust
//! // 1. 声明接口（对应前端同步函数 window.mermaidTool / window.switchTheme）
//! frontend_api!(FrontendApi,
//!     "mermaidTool" => mermaid_tool(code: &str, mode: &str) -> serde_json::Value,
//!     "switchTheme" => switch_theme(theme: &str),
//! );
//!
//! // 2. 一次初始化
//! let api = FrontendApi::new(Arc::new(invoker_from_app(app.handle())?));
//!
//! // 3. 直接调用（参数/返回自动转换，无感知）
//! let result = api.mermaid_tool("flowchart LR A-->B", "render").await?;
//! api.switch_theme("dark").await?;
//! ```
//!
//! 说明：本模块为公共调用封装库，`FrontendScriptInvoker`/`TauriWebviewInvoker`/`FrontendService`/
//! `frontend_api!` 及示例 `FrontendApi` 均为预留公共 API，接入具体业务后即自然被使用；
//! 在此统一允许未使用告警（当前仅 `global_invoker`/`init_global_invoker` 被内部工具消费）。
#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tauri::{Listener, Manager, WebviewWindow};

/// 响应事件名递增序号（保证同窗口多次调用事件名唯一）
static REQ_SEQ: AtomicU64 = AtomicU64::new(0);

/// 默认等待前端返回的超时时间
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// 前端脚本调用抽象（依赖倒置：业务依赖此 trait，而非直接绑定 `WebviewWindow`）
///
/// 方法返回 boxed future 以支持 trait 对象（`dyn FrontendScriptInvoker`）。
/// `args` 为**已转义好的 JS 字面量字符串**（序列化由调用方负责，本 trait 只负责脚本构造）。
pub trait FrontendScriptInvoker: Send + Sync {
    /// 调用前端全局函数 `window.[func_name](...args)` 并获取其返回值
    fn call_global_fn<'a>(
        &'a self,
        func_name: &'a str,
        args: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>>;
}

/// Tauri Webview 唯一正式实现（单一职责：只负责构造脚本 + eval + 事件回调）
#[derive(Clone)]
pub struct TauriWebviewInvoker {
    window: WebviewWindow,
}

impl TauriWebviewInvoker {
    pub fn new(window: WebviewWindow) -> Self {
        Self { window }
    }
}

impl FrontendScriptInvoker for TauriWebviewInvoker {
    fn call_global_fn<'a>(
        &'a self,
        func_name: &'a str,
        args: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value>> + Send + 'a>> {
        Box::pin(async move {
            let req_id = REQ_SEQ.fetch_add(1, Ordering::Relaxed);
            let resp_event = format!("__frontend_invoke_resp_{req_id}");

            // 参数已是安全转义的 JS 字面量，直接拼接（无二次序列化）
            let args_expr = args.join(",");

            let app = self.window.app_handle().clone();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

            // 1. 注册一次性响应监听（回调同步，UnboundedSender::send 为 &self 不消费）
            let listener = app.listen_any(&resp_event, move |event| {
                let _ = tx.send(event.payload().to_string());
            });

            // 2. eval 脚本：同步调用前端函数（要求 window.<fn> 为同步方法）→ 返回值或异常统一经事件回传
            let script = format!(
                "try {{ \
                   const __r = window.{fn}({args}); \
                   window.__TAURI__.event.emit('{evt}', JSON.stringify({{ ok: true, value: __r }})); \
                 }} catch (__e) {{ \
                   window.__TAURI__.event.emit('{evt}', JSON.stringify({{ ok: false, error: String(__e && __e.message || __e) }})); \
                 }}",
                fn = func_name,
                args = args_expr,
                evt = resp_event
            );
            // Tauri 2.x 默认注入 window.__TAURI__（withGlobalTauri 默认开启），直接 eval
            self.window
                .eval(&script)
                .context(format!("前端脚本执行失败: window.{func_name}"))?;

            // 3. 等待回调（超时视为前端无此函数或执行超时）
            let payload = match tokio::time::timeout(DEFAULT_TIMEOUT, rx.recv()).await {
                Ok(Some(p)) => p,
                Ok(None) => bail!("前端响应通道已关闭"),
                Err(_) => {
                    bail!(
                        "等待前端返回超时（{}s）：window.{func_name} 可能未定义",
                        DEFAULT_TIMEOUT.as_secs()
                    )
                }
            };
            app.unlisten(listener);

            // 4. 解析统一响应结构 { ok, value | error }
            let resp: serde_json::Value = serde_json::from_str(&payload)
                .with_context(|| format!("前端响应解析失败: {payload}"))?;
            if resp["ok"].as_bool().unwrap_or(false) {
                Ok(resp
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null))
            } else {
                bail!("前端函数 window.{func_name} 执行出错: {}", resp["error"])
            }
        })
    }
}

/// 统一服务入口（依赖注入）
#[derive(Clone)]
pub struct FrontendService {
    invoker: std::sync::Arc<dyn FrontendScriptInvoker>,
}

impl FrontendService {
    pub fn new(invoker: std::sync::Arc<dyn FrontendScriptInvoker>) -> Self {
        Self { invoker }
    }

    /// 入口：调用前端全局函数 `window.[fn_name](...args)`，返回前端返回值
    /// （便捷入口：参数为 `serde_json::Value`，内部转为 JS 字面量；需极致性能请用宏生成 API）
    pub async fn call(
        &self,
        fn_name: &str,
        args: &[serde_json::Value],
    ) -> Result<serde_json::Value> {
        let arg_strs: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.invoker.call_global_fn(fn_name, &arg_strs).await
    }
}

/// 便捷构建：从 AppHandle 获取主窗口的调用器
pub fn invoker_from_app(app: &tauri::AppHandle) -> Result<TauriWebviewInvoker> {
    let window = app
        .get_webview_window("main")
        .context("找不到主窗口（label: main）")?;
    Ok(TauriWebviewInvoker::new(window))
}

// ===== 全局单例（供工具闭包等无法直接持有 AppHandle 的场景使用） =====

static GLOBAL_INVOKER: OnceLock<std::sync::Arc<dyn FrontendScriptInvoker>> = OnceLock::new();

/// 初始化全局前端调用器（应用 setup 阶段调用一次；重复调用忽略）
pub fn init_global_invoker(app: &tauri::AppHandle) -> Result<()> {
    let invoker = invoker_from_app(app)?;
    let _ = GLOBAL_INVOKER.set(std::sync::Arc::new(invoker));
    Ok(())
}

/// 获取全局前端调用器（未初始化返回 `None`）
pub fn global_invoker() -> Option<&'static dyn FrontendScriptInvoker> {
    GLOBAL_INVOKER.get().map(|i| i.as_ref())
}

// ===== 声明式 API 宏（OpenFeign 风格） =====

/// 参数序列化辅助：任意 `Serialize` 值直接序列化为 JS 字面量字符串
///
/// 直接 `to_string` 一步到位（跳过 `Value` 树中间层）；基元类型（str/数字/bool）
/// 由 serde_json 零中间开销直写。失败时降级为 `null`，不中断调用。
#[doc(hidden)]
pub fn __api_arg<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

/// 返回值反序列化辅助：JSON → 目标类型
#[doc(hidden)]
pub fn __api_parse_value<T: serde::de::DeserializeOwned>(ret: serde_json::Value) -> Result<T> {
    serde_json::from_value(ret)
        .map_err(|e| anyhow::anyhow!("前端返回值反序列化失败: {e}"))
}

/// 返回类型归一化辅助（未声明 `-> T` 时默认为 `()`）
#[doc(hidden)]
#[macro_export]
macro_rules! __api_ret {
    ($ty:ty) => {
        $ty
    };
    () => {
        ()
    };
}

/// 返回值解析辅助（按返回类型分发反序列化）
#[doc(hidden)]
#[macro_export]
macro_rules! __api_parse {
    ($ret:expr, $ty:ty) => {
        $crate::core::frontend_invoker::__api_parse_value::<$ty>($ret)
    };
    ($ret:expr,) => {
        $crate::core::frontend_invoker::__api_parse_value::<()>($ret)
    };
}

/// 声明式前端 API（OpenFeign 风格，调用方无感知）
///
/// 一次声明接口清单，宏展开为完整方法实现：参数自动 JSON 序列化、返回自动反序列化。
///
/// 语法：
/// ```text
/// frontend_api!(ApiStructName,
///     "前端函数名" => 方法名(参数名: 类型, ...) -> 返回类型,   // 返回类型可省略，默认 ()
///     ...
/// );
/// ```
///
/// 要求前端对应函数为**同步方法**，挂载于 `window` 上。
#[macro_export]
macro_rules! frontend_api {
    (
        $api_name:ident,
        $(
            $(#[$meta:meta])*
            $fn_name:literal => $method:ident ( $($arg:ident : $arg_ty:ty),* ) $(-> $ret:ty)?
        ),+ $(,)?
    ) => {
        /// 声明式前端 API（由 `frontend_api!` 宏生成）：方法参数自动序列化传入、返回值自动反序列化
        pub struct $api_name {
            invoker: std::sync::Arc<dyn $crate::core::frontend_invoker::FrontendScriptInvoker>,
        }

        impl $api_name {
            /// 使用前端调用器构建 API 实例
            pub fn new(
                invoker: std::sync::Arc<dyn $crate::core::frontend_invoker::FrontendScriptInvoker>,
            ) -> Self {
                Self { invoker }
            }

            $(
                $(#[$meta])*
                pub async fn $method(
                    &self,
                    $($arg: $arg_ty),*
                ) -> ::anyhow::Result<$crate::__api_ret!($($ret)?)> {
                    let __ret = self
                        .invoker
                        .call_global_fn(
                            $fn_name,
                            &[$($crate::core::frontend_invoker::__api_arg(&$arg)),*],
                        )
                        .await?;
                    $crate::__api_parse!(__ret, $($ret)?)
                }
            )+
        }
    };
}

// ===== 示例：声明式前端 API（可直接使用，用法见模块文档） =====
frontend_api!(FrontendApi,
    /// 处理 mermaid 图表（check=校验语法 / render=渲染，前端 window.mermaidTool 为同步函数）
    "mermaidTool" => mermaid_tool(code: &str, mode: &str) -> serde_json::Value,
);
