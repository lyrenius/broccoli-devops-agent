//! Typed tools and the allowlisting registry.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{HarnessError, HarnessResult};

/// Description of one tool as presented to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name the model calls.
    pub name: String,
    /// What the tool does and when to use it.
    pub description: String,
    /// JSON-schema description of the arguments object.
    pub parameters: Value,
    /// Whether a successful call ends the run with the call's validated value as the result.
    ///
    /// Terminal tools are how adapters demand structured final output instead of parsing prose.
    pub terminal: bool,
}

/// A tool result: `Ok` carries the output value, `Err` carries a message the model may read and
/// recover from. Handler errors are model-visible by design; harness-fatal failures do not belong
/// here.
pub type ToolResult = Result<Value, String>;

/// Executable side of a tool.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Validates and executes one call with the model-supplied arguments.
    async fn call(&self, arguments: Value) -> ToolResult;

    /// Stops disposable futures by default. Resource-owning handlers override this and return
    /// only after their child processes and output have been cleaned up.
    async fn call_with_cancel(
        &self,
        arguments: Value,
        mut cancel: crate::CancelToken,
    ) -> ToolResult {
        tokio::select! { biased;
            () = cancel.cancelled() => Err("tool cancelled".into()),
            result = self.call(arguments) => result,
        }
    }
}

/// A registered tool: its model-facing spec plus its handler.
#[derive(Clone)]
pub struct Tool {
    /// Model-facing description.
    pub spec: ToolSpec,
    /// Executable handler.
    pub handler: Arc<dyn ToolHandler>,
}

/// Wraps an async closure as a [`ToolHandler`].
pub fn tool_fn<F, Fut>(f: F) -> Arc<dyn ToolHandler>
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolResult> + Send + 'static,
{
    struct FnTool<F>(F);

    #[async_trait]
    impl<F, Fut> ToolHandler for FnTool<F>
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        async fn call(&self, arguments: Value) -> ToolResult {
            let fut: Pin<Box<dyn Future<Output = ToolResult> + Send>> =
                Box::pin((self.0)(arguments));
            fut.await
        }
    }

    Arc::new(FnTool(f))
}

/// Wraps a resource-owning handler. It must observe the supplied signal and finish its cleanup;
/// the loop waits for that result on cancellation and timeout rather than dropping the owner.
pub fn tool_fn_cancellable<F, Fut>(f: F) -> Arc<dyn ToolHandler>
where
    F: Fn(Value, crate::CancelToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolResult> + Send + 'static,
{
    struct Cooperative<F>(F);
    #[async_trait]
    impl<F, Fut> ToolHandler for Cooperative<F>
    where
        F: Fn(Value, crate::CancelToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        async fn call(&self, arguments: Value) -> ToolResult {
            let (_handle, token) = crate::cancel_pair();
            (self.0)(arguments, token).await
        }
        async fn call_with_cancel(
            &self,
            arguments: Value,
            cancel: crate::CancelToken,
        ) -> ToolResult {
            (self.0)(arguments, cancel).await
        }
    }
    Arc::new(Cooperative(f))
}

/// The allowlist of tools available to one agent run.
///
/// The registry is the enforcement point: a model can only reach handlers registered here, and an
/// unknown tool name becomes a model-visible error output, never an execution.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Tool>,
    order: Vec<String>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one tool; duplicate names are an error rather than a silent override.
    pub fn register(&mut self, spec: ToolSpec, handler: Arc<dyn ToolHandler>) -> HarnessResult<()> {
        if self.tools.contains_key(&spec.name) {
            return Err(HarnessError::DuplicateTool(spec.name));
        }
        self.order.push(spec.name.clone());
        self.tools.insert(spec.name.clone(), Tool { spec, handler });
        Ok(())
    }

    /// Returns the specs in registration order, for the model request.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|name| self.tools.get(name).map(|tool| tool.spec.clone()))
            .collect()
    }

    /// Looks up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }
}
