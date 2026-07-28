//! 与具体游戏引擎无关的可信 Lua Host 基础契约。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::diagnostic::SafeDiagnostic;

/// Host 异步调用返回的受管 future。
pub(crate) type TrustedLuaHostFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Lua 能通过 `pcall` 检查的 Host 错误事实。
///
/// `domain` 与 `kind` 是稳定的机器字段；`message` 只用于人类诊断。
#[derive(Clone, Debug)]
pub(crate) struct TrustedLuaHostCallError {
    domain: &'static str,
    kind: &'static str,
    operation: Option<&'static str>,
    message: String,
    retry_after_ms: Option<u64>,
    safe_diagnostic: Option<Box<SafeDiagnostic>>,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl TrustedLuaHostCallError {
    pub(crate) fn new(
        domain: &'static str,
        kind: &'static str,
        message: impl Into<String>,
        retry_after_ms: Option<u64>,
        source: Option<Arc<dyn Error + Send + Sync>>,
    ) -> Self {
        Self {
            domain,
            kind,
            operation: None,
            message: message.into(),
            retry_after_ms,
            safe_diagnostic: None,
            source,
        }
    }

    /// 补充 Lua Host 公开 API 的稳定操作名；不得放入 SQL、参数或用户正文。
    pub(crate) fn with_operation(mut self, operation: &'static str) -> Self {
        self.operation = Some(operation);
        self
    }

    /// 保存错误根在仍持有类型化事实时生成的安全公开投影。
    pub(crate) fn with_safe_diagnostic(mut self, diagnostic: SafeDiagnostic) -> Self {
        self.safe_diagnostic = Some(Box::new(diagnostic));
        self
    }

    pub(crate) const fn domain(&self) -> &'static str {
        self.domain
    }

    pub(crate) const fn kind(&self) -> &'static str {
        self.kind
    }

    pub(crate) const fn operation(&self) -> Option<&'static str> {
        self.operation
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }

    pub(crate) fn safe_diagnostic(&self) -> Option<&SafeDiagnostic> {
        self.safe_diagnostic.as_deref()
    }
}

impl fmt::Display for TrustedLuaHostCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TrustedLuaHostCallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}
