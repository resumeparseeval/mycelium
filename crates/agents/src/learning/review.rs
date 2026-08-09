//! The background self-review fork.
//!
//! Replays a snapshot of the conversation through a second agent run that can
//! only call memory and skill tools, asking it to persist anything worth
//! learning. The fork never touches the live session: its transcript is
//! discarded and only the durable writes (memory entries, skill files) remain.

use std::sync::{Arc, Mutex};

use {
    anyhow::Result,
    tracing::{info, warn},
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels};

use {
    super::{counters::ReviewKind, prompts::review_prompt},
    crate::{
        model::{ChatMessage, LlmProvider, UserContent},
        runner::{AgentLoopLimits, RunnerEvent, run_agent_loop_with_context_and_limits},
        tool_registry::ToolRegistry,
    },
};

/// Tools the review fork may call. Everything else is stripped from its
/// registry, so a denied call can never reach a real tool implementation.
///
/// `delete_skill` is deliberately absent: destructive skill operations stay
/// foreground-only until provenance guards land.
pub const REVIEW_TOOL_ALLOWLIST: &[&str] = &[
    "memory_save",
    "memory_search",
    "memory_get",
    "memory_delete",
    "read_skill",
    "create_skill",
    "update_skill",
    "patch_skill",
    "write_skill_files",
];

/// Default iteration cap for the fork (matches hermes-agent).
pub const DEFAULT_REVIEW_MAX_ITERATIONS: usize = 16;

const REVIEW_SYSTEM_PROMPT: &str = r"You are the self-improvement review process of an AI agent, running after a conversation turn completed. You review the transcript below and persist durable learnings using only the tools provided. Your text output is discarded — only tool calls have effect.";

/// One successful write performed by the fork.
#[derive(Debug, Clone)]
pub struct ReviewAction {
    pub tool: String,
    pub detail: String,
}

/// Result of a completed review run.
#[derive(Debug, Default)]
pub struct ReviewOutcome {
    pub actions: Vec<ReviewAction>,
    pub iterations: usize,
}

impl ReviewOutcome {
    /// One-line summary for surfacing to the user, or `None` if no writes.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.actions.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .actions
            .iter()
            .map(|action| format!("{} ({})", action.tool, action.detail))
            .collect();
        Some(format!("Self-improvement review: {}", parts.join(", ")))
    }
}

fn summarize_arguments(name: &str, arguments: &serde_json::Value) -> String {
    let named = arguments
        .get("name")
        .or_else(|| arguments.get("skill_name"))
        .or_else(|| arguments.get("path"))
        .and_then(serde_json::Value::as_str);
    if let Some(target) = named {
        return target.to_string();
    }
    let chars = arguments
        .get("content")
        .or_else(|| arguments.get("text"))
        .and_then(serde_json::Value::as_str)
        .map(str::len);
    match chars {
        Some(chars) => format!("{chars} chars"),
        None => name.to_string(),
    }
}

fn format_transcript(conversation: &[ChatMessage]) -> String {
    let mut text = String::new();
    for msg in conversation {
        let (role, content) = match msg {
            ChatMessage::System { .. } => continue,
            ChatMessage::User {
                content: UserContent::Text(t),
                ..
            } => ("user", t.as_str()),
            ChatMessage::User {
                content: UserContent::Multimodal(_),
                ..
            } => ("user", "[multimodal content]"),
            ChatMessage::Assistant { content, .. } => {
                ("assistant", content.as_deref().unwrap_or(""))
            },
            ChatMessage::Tool { content, .. } => ("tool", content.as_str()),
        };
        let truncated = &content[..content.floor_char_boundary(2000.min(content.len()))];
        text.push_str(&format!("{role}: {truncated}\n\n"));
    }
    text
}

/// Run the self-review fork over a conversation snapshot.
///
/// `tools` is the turn's full registry; it is filtered to
/// [`REVIEW_TOOL_ALLOWLIST`] before the fork sees it. `skills_index` is the
/// skill catalog shown to the fork so it can prefer patching existing skills.
///
/// Errors from the LLM run are swallowed (logged) — a failed review must never
/// surface as a turn failure. Only tool failures inside a successful run are
/// reflected in the returned actions.
#[tracing::instrument(skip_all, fields(kind = ?kind))]
pub async fn run_learning_review(
    provider: Arc<dyn LlmProvider>,
    tools: &ToolRegistry,
    conversation: &[ChatMessage],
    kind: ReviewKind,
    skills_index: Option<&str>,
    max_iterations: usize,
) -> Result<ReviewOutcome> {
    let registry = tools.clone_allowed_by(|name| REVIEW_TOOL_ALLOWLIST.contains(&name));
    if registry.is_empty() {
        info!("learning review skipped: no whitelisted tools available");
        return Ok(ReviewOutcome::default());
    }

    let mut system_prompt = REVIEW_SYSTEM_PROMPT.to_string();
    if let Some(index) = skills_index
        && !index.trim().is_empty()
    {
        system_prompt.push_str("\n\nCurrent skill library:\n");
        system_prompt.push_str(index);
    }

    let user_content = UserContent::Text(format!(
        "{}\n\n---\n\n{}",
        format_transcript(conversation),
        review_prompt(kind)
    ));

    // Collect successful writes via runner events: arguments arrive on
    // ToolCallStart, success on ToolCallEnd.
    let pending: Arc<Mutex<std::collections::HashMap<String, ReviewAction>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let actions: Arc<Mutex<Vec<ReviewAction>>> = Arc::new(Mutex::new(Vec::new()));
    let on_event: crate::runner::OnEvent = {
        let pending = Arc::clone(&pending);
        let actions = Arc::clone(&actions);
        Box::new(move |event| match event {
            RunnerEvent::ToolCallStart {
                id,
                name,
                arguments,
                ..
            } => {
                let detail = summarize_arguments(&name, &arguments);
                pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, ReviewAction { tool: name, detail });
            },
            RunnerEvent::ToolCallEnd { id, success, .. } => {
                let action = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if success
                    && let Some(action) = action
                    && action.tool != "memory_search"
                    && action.tool != "memory_get"
                    && action.tool != "read_skill"
                {
                    actions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(action);
                }
            },
            _ => {},
        })
    };

    let result = run_agent_loop_with_context_and_limits(
        provider,
        &registry,
        &system_prompt,
        &user_content,
        Some(&on_event),
        None,
        None,
        None,
        None,
        AgentLoopLimits {
            max_iterations: Some(max_iterations),
        },
    )
    .await;

    let actions = std::mem::take(&mut *actions.lock().unwrap_or_else(|e| e.into_inner()));
    match result {
        Ok(run) => {
            #[cfg(feature = "metrics")]
            {
                counter!("learning_reviews_total", labels::SUCCESS => "true").increment(1);
                counter!("learning_review_actions_total").increment(actions.len() as u64);
            }
            info!(
                actions = actions.len(),
                iterations = run.iterations,
                "learning review complete"
            );
            Ok(ReviewOutcome {
                actions,
                iterations: run.iterations,
            })
        },
        Err(error) => {
            #[cfg(feature = "metrics")]
            counter!("learning_reviews_total", labels::SUCCESS => "false").increment(1);
            warn!(%error, "learning review failed");
            Ok(ReviewOutcome {
                actions,
                iterations: 0,
            })
        },
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            model::{CompletionResponse, StreamEvent, ToolCall, Usage},
            tool_registry::AgentTool,
        },
        std::pin::Pin,
        tokio_stream::Stream,
    };

    struct RecordingTool {
        name: &'static str,
        calls: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait::async_trait]
    impl AgentTool for RecordingTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, params: serde_json::Value) -> Result<serde_json::Value> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params);
            Ok(serde_json::json!({"ok": true}))
        }
    }

    /// Provider that calls the given tool once, then stops.
    struct OneToolCallProvider {
        tool: &'static str,
        arguments: serde_json::Value,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmProvider for OneToolCallProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn id(&self) -> &str {
            "mock-model"
        }

        fn supports_tools(&self) -> bool {
            true
        }

        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> Result<CompletionResponse> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Ok(CompletionResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: self.tool.into(),
                        arguments: self.arguments.clone(),
                        argument_diagnostic: None,
                        metadata: None,
                    }],
                    usage: Usage::default(),
                })
            } else {
                Ok(CompletionResponse {
                    text: Some("Nothing to save.".into()),
                    tool_calls: vec![],
                    usage: Usage::default(),
                })
            }
        }

        fn stream(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::empty())
        }
    }

    fn registry_with(tools: Vec<Box<dyn AgentTool>>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for tool in tools {
            registry.register(tool);
        }
        registry
    }

    #[tokio::test]
    async fn review_records_successful_writes() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with(vec![Box::new(RecordingTool {
            name: "create_skill",
            calls: Arc::clone(&calls),
        })]);
        let provider = Arc::new(OneToolCallProvider {
            tool: "create_skill",
            arguments: serde_json::json!({"name": "rust-testing", "content": "..."}),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });

        let outcome = run_learning_review(
            provider,
            &registry,
            &[ChatMessage::user("we discovered a testing workflow")],
            ReviewKind::Skills,
            Some("- rust-testing: how to test rust code"),
            DEFAULT_REVIEW_MAX_ITERATIONS,
        )
        .await
        .unwrap();

        assert_eq!(outcome.actions.len(), 1);
        assert_eq!(outcome.actions[0].tool, "create_skill");
        assert_eq!(outcome.actions[0].detail, "rust-testing");
        assert_eq!(calls.lock().unwrap().len(), 1);
        let summary = outcome.summary().unwrap();
        assert!(summary.contains("create_skill (rust-testing)"));
    }

    #[tokio::test]
    async fn non_whitelisted_tools_are_stripped_from_the_fork() {
        let dangerous_calls = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with(vec![
            Box::new(RecordingTool {
                name: "exec",
                calls: Arc::clone(&dangerous_calls),
            }),
            Box::new(RecordingTool {
                name: "memory_save",
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
        ]);
        // Provider tries to call exec — the tool is absent from the fork's
        // registry, so the call fails and no real tool runs.
        let provider = Arc::new(OneToolCallProvider {
            tool: "exec",
            arguments: serde_json::json!({"command": "rm -rf /"}),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });

        let outcome = run_learning_review(
            provider,
            &registry,
            &[ChatMessage::user("test")],
            ReviewKind::Combined,
            None,
            DEFAULT_REVIEW_MAX_ITERATIONS,
        )
        .await
        .unwrap();

        assert!(dangerous_calls.lock().unwrap().is_empty());
        assert!(outcome.actions.is_empty());
    }

    #[tokio::test]
    async fn empty_registry_short_circuits() {
        let registry = registry_with(vec![Box::new(RecordingTool {
            name: "exec",
            calls: Arc::new(Mutex::new(Vec::new())),
        })]);
        let provider = Arc::new(OneToolCallProvider {
            tool: "memory_save",
            arguments: serde_json::json!({}),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });

        let outcome = run_learning_review(
            provider,
            &registry,
            &[ChatMessage::user("test")],
            ReviewKind::Memory,
            None,
            DEFAULT_REVIEW_MAX_ITERATIONS,
        )
        .await
        .unwrap();
        assert!(outcome.actions.is_empty());
        assert_eq!(outcome.iterations, 0);
    }

    #[tokio::test]
    async fn reads_are_not_reported_as_actions() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let registry = registry_with(vec![Box::new(RecordingTool {
            name: "read_skill",
            calls: Arc::clone(&calls),
        })]);
        let provider = Arc::new(OneToolCallProvider {
            tool: "read_skill",
            arguments: serde_json::json!({"name": "rust-testing"}),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });

        let outcome = run_learning_review(
            provider,
            &registry,
            &[ChatMessage::user("test")],
            ReviewKind::Skills,
            None,
            DEFAULT_REVIEW_MAX_ITERATIONS,
        )
        .await
        .unwrap();

        // The read executed but is not a persisted learning action.
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert!(outcome.actions.is_empty());
        assert!(outcome.summary().is_none());
    }
}
