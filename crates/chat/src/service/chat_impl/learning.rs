//! Post-turn learning hooks.
//!
//! After each successful, persisted turn either the counter-triggered
//! self-review fork runs (`[learning]` enabled, the default) or the legacy
//! interval-based silent memory extraction (`[learning]` disabled). Both are
//! fire-and-forget background tasks: they must never delay or fail the turn.

use std::sync::Arc;

use {
    tokio::sync::RwLock,
    tracing::{debug, info, warn},
};

use {
    moltis_agents::{
        learning::{self, LearningCounters},
        model::{LlmProvider, values_to_chat_messages_with_tool_result_limit},
        tool_registry::ToolRegistry,
    },
    moltis_config::{AgentMemoryWriteMode, LearningConfig},
    moltis_sessions::{state_store::SessionStateStore, store::SessionStore},
    moltis_skills::types::SkillMetadata,
};

use crate::{
    memory_tools::AgentScopedMemoryWriter, runtime::ChatRuntime,
    types::memory_write_mode_allows_save,
};

const STATE_NAMESPACE: &str = "learning";
const STATE_KEY: &str = "counters";
/// Conversation snapshot size handed to the review fork.
const REVIEW_WINDOW_MESSAGES: usize = 40;

/// Everything the post-turn learning path needs from the finished turn.
pub(crate) struct PostTurnLearning<'a> {
    pub state: &'a Arc<dyn ChatRuntime>,
    pub session_store: &'a Arc<SessionStore>,
    pub session_state_store: Option<&'a Arc<SessionStateStore>>,
    pub tool_registry: &'a Arc<RwLock<ToolRegistry>>,
    pub provider: &'a Arc<dyn LlmProvider>,
    pub discovered_skills: &'a [SkillMetadata],
    pub session_key: &'a str,
    pub session_agent_id: &'a str,
    /// Index of this turn's user message in the persisted transcript.
    pub user_message_index: usize,
    /// Total persisted messages after this turn.
    pub message_count: u32,
    pub learning: LearningConfig,
    pub auto_extract_interval: u32,
    pub write_mode: AgentMemoryWriteMode,
    pub max_tool_result_bytes: usize,
}

/// Run whichever post-turn learning path is configured.
pub(crate) async fn run_post_turn_learning(args: PostTurnLearning<'_>) {
    if args.learning.enabled {
        review_fork_path(args).await;
    } else {
        legacy_extract_path(args).await;
    }
}

/// Tool activity in this turn, derived from the persisted transcript tail.
///
/// Iterations are counted from tool-result rows; write classification also
/// scans assistant `tool_calls`. A foreground write attempt counts as a write
/// even if the tool errored — slightly lenient, but it means the agent was
/// already engaging with the store this turn.
fn turn_tool_usage(messages: &[serde_json::Value], from_index: usize) -> (u32, bool, bool) {
    let mut iterations = 0u32;
    let mut wrote_memory = false;
    let mut wrote_skill = false;
    let mut classify = |name: &str| {
        wrote_memory |= learning::is_memory_write_tool(name);
        wrote_skill |= learning::is_skill_write_tool(name);
    };
    for msg in messages.iter().skip(from_index) {
        match msg.get("role").and_then(serde_json::Value::as_str) {
            Some("tool" | "tool_result") => {
                iterations = iterations.saturating_add(1);
                if let Some(name) = msg.get("tool_name").and_then(serde_json::Value::as_str) {
                    classify(name);
                }
            },
            Some("assistant") => {
                let calls = msg
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                for call in calls {
                    if let Some(name) = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(serde_json::Value::as_str)
                    {
                        classify(name);
                    }
                }
            },
            _ => {},
        }
    }
    (iterations, wrote_memory, wrote_skill)
}

async fn load_counters(store: &SessionStateStore, session_key: &str) -> LearningCounters {
    match store.get(session_key, STATE_NAMESPACE, STATE_KEY).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => LearningCounters::default(),
    }
}

async fn save_counters(store: &SessionStateStore, session_key: &str, counters: LearningCounters) {
    let Ok(raw) = serde_json::to_string(&counters) else {
        return;
    };
    if let Err(error) = store
        .set(session_key, STATE_NAMESPACE, STATE_KEY, &raw)
        .await
    {
        warn!(%error, "failed to persist learning counters");
    }
}

async fn review_fork_path(args: PostTurnLearning<'_>) {
    let Some(state_store) = args.session_state_store else {
        debug!("learning review skipped: no session state store");
        return;
    };

    let messages = match args.session_store.read(args.session_key).await {
        Ok(messages) => messages,
        Err(error) => {
            warn!(%error, "learning review skipped: failed to read transcript");
            return;
        },
    };
    let (iterations, wrote_memory, wrote_skill) =
        turn_tool_usage(&messages, args.user_message_index);

    let mut counters = load_counters(state_store, args.session_key).await;
    counters.record_turn(iterations, wrote_memory, wrote_skill);

    let due = counters.due(
        args.learning.memory_review_interval,
        args.learning.skill_review_interval,
    );
    if let Some(kind) = due {
        let registry = args
            .tool_registry
            .read()
            .await
            .clone_allowed_by(|name| learning::review::REVIEW_TOOL_ALLOWLIST.contains(&name));
        if registry.is_empty() {
            debug!("learning review skipped: no memory or skill tools registered");
        } else {
            counters.reset_for(kind);

            let recent_start = messages.len().saturating_sub(REVIEW_WINDOW_MESSAGES);
            let conversation = values_to_chat_messages_with_tool_result_limit(
                &messages[recent_start..],
                args.max_tool_result_bytes,
            );
            let skills_index = args
                .discovered_skills
                .iter()
                .map(|skill| format!("- {}: {}", skill.name, skill.description))
                .collect::<Vec<_>>()
                .join("\n");
            let provider = Arc::clone(args.provider);
            let session_key = args.session_key.to_string();
            let max_iterations = args.learning.max_review_iterations;

            tokio::spawn(async move {
                match learning::run_learning_review(
                    provider,
                    &registry,
                    &conversation,
                    kind,
                    (!skills_index.is_empty()).then_some(skills_index.as_str()),
                    max_iterations,
                )
                .await
                {
                    Ok(outcome) => {
                        if let Some(summary) = outcome.summary() {
                            info!(session = %session_key, %summary, "learning review persisted");
                        }
                    },
                    Err(error) => warn!(session = %session_key, %error, "learning review failed"),
                }
            });
        }
    }

    save_counters(state_store, args.session_key, counters).await;
}

/// Legacy interval-based silent memory extraction (pre-learning-loop path).
async fn legacy_extract_path(args: PostTurnLearning<'_>) {
    let interval = args.auto_extract_interval;
    // A "turn" = user + assistant = 2 messages.
    let turn_number = args.message_count / 2;
    let due = interval > 0
        && turn_number > 0
        && turn_number.is_multiple_of(interval)
        && memory_write_mode_allows_save(args.write_mode);
    if !due {
        return;
    }
    let Some(mm) = args.state.memory_manager() else {
        return;
    };

    let window = (interval as usize) * 2;
    let recent: Vec<serde_json::Value> = match args.session_store.read(args.session_key).await {
        Ok(history) => {
            let start = history.len().saturating_sub(window);
            history.into_iter().skip(start).collect()
        },
        Err(_) => Vec::new(),
    };
    if recent.is_empty() {
        return;
    }

    let chat_msgs =
        values_to_chat_messages_with_tool_result_limit(&recent, args.max_tool_result_bytes);
    let agent_id = args.session_agent_id.to_string();
    let mm = Arc::clone(mm);
    let write_mode = args.write_mode;
    let provider = Arc::clone(args.provider);
    tokio::spawn(async move {
        let writer: Arc<dyn moltis_agents::memory_writer::MemoryWriter> =
            Arc::new(AgentScopedMemoryWriter::new(mm, agent_id, write_mode));
        match moltis_agents::silent_turn::run_silent_memory_turn_with_prompt(
            provider,
            &chat_msgs,
            writer,
            moltis_agents::silent_turn::SilentTurnPrompt::PeriodicExtract,
        )
        .await
        {
            Ok(paths) if !paths.is_empty() => {
                info!(
                    files = paths.len(),
                    turn = turn_number,
                    "periodic memory extraction: wrote files"
                );
            },
            Ok(_) => {},
            Err(error) => {
                warn!(%error, "periodic memory extraction failed");
            },
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut value = serde_json::json!({"role": role, "content": "x"});
        if let (Some(obj), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        value
    }

    #[test]
    fn turn_usage_counts_iterations_and_classifies_writes() {
        let messages = vec![
            msg("user", serde_json::json!({})),
            msg(
                "assistant",
                serde_json::json!({"tool_calls": [
                    {"id": "1", "type": "function", "function": {"name": "memory_save", "arguments": "{}"}},
                ]}),
            ),
            msg("tool", serde_json::json!({"tool_call_id": "1"})),
            msg(
                "tool_result",
                serde_json::json!({"tool_call_id": "2", "tool_name": "patch_skill"}),
            ),
            msg("assistant", serde_json::json!({})),
        ];

        let (iterations, wrote_memory, wrote_skill) = turn_tool_usage(&messages, 0);
        assert_eq!(iterations, 2);
        assert!(wrote_memory);
        assert!(wrote_skill);
    }

    #[test]
    fn turn_usage_ignores_messages_before_the_turn() {
        let messages = vec![
            msg(
                "tool_result",
                serde_json::json!({"tool_call_id": "1", "tool_name": "memory_save"}),
            ),
            msg("user", serde_json::json!({})),
            msg("assistant", serde_json::json!({})),
        ];
        let (iterations, wrote_memory, wrote_skill) = turn_tool_usage(&messages, 1);
        assert_eq!(iterations, 0);
        assert!(!wrote_memory);
        assert!(!wrote_skill);
    }

    #[test]
    fn turn_usage_without_tools_is_a_plain_turn() {
        let messages = vec![
            msg("user", serde_json::json!({})),
            msg("assistant", serde_json::json!({})),
        ];
        let (iterations, wrote_memory, wrote_skill) = turn_tool_usage(&messages, 0);
        assert_eq!(iterations, 0);
        assert!(!wrote_memory);
        assert!(!wrote_skill);
    }
}
