//! Session communication tools for listing, inspecting, and messaging sessions.
//!
//! These tools expose cross-session coordination primitives:
//! - `sessions_list`: list sessions with optional filtering
//! - `sessions_history`: read paginated history from a session
//! - `sessions_search`: search past session history for relevant snippets
//! - `sessions_send`: send a message to another session (async or sync)

use std::{collections::HashMap, sync::Arc};

use {async_trait::async_trait, futures::future::BoxFuture, serde_json::Value};

use {
    moltis_agents::tool_registry::AgentTool,
    moltis_sessions::{metadata::SqliteSessionMetadata, store::SessionStore},
};

use crate::{
    Error,
    params::{bool_param, owned_str_param, require_str, str_param, u64_param},
};

/// Request payload for cross-session message delivery.
#[derive(Debug, Clone)]
pub struct SendToSessionRequest {
    pub key: String,
    pub message: String,
    pub wait_for_reply: bool,
    pub model: Option<String>,
}

/// Callback used by `sessions_send`.
pub type SendToSessionFn =
    Arc<dyn Fn(SendToSessionRequest) -> BoxFuture<'static, crate::Result<Value>> + Send + Sync>;

/// Policy controlling which sessions an agent can access.
#[derive(Debug, Clone, Default)]
pub struct SessionAccessPolicy {
    /// If set, only sessions with keys matching this prefix are visible.
    pub key_prefix: Option<String>,
    /// Explicit list of session keys this agent can access (in addition to prefix).
    pub allowed_keys: Vec<String>,
    /// If true, agent can send messages to other sessions.
    pub can_send: bool,
    /// If true, agent can access sessions from other agents.
    pub cross_agent: bool,
}

impl SessionAccessPolicy {
    /// Check if a session key is accessible under this policy.
    pub fn can_access(&self, key: &str) -> bool {
        // Check explicit allowed keys first.
        if self.allowed_keys.iter().any(|k| k == key) {
            return true;
        }

        // Check prefix match.
        if let Some(ref prefix) = self.key_prefix {
            return key.starts_with(prefix);
        }

        // Default: allow all if no restrictions.
        true
    }
}

impl From<&moltis_config::SessionAccessPolicyConfig> for SessionAccessPolicy {
    fn from(config: &moltis_config::SessionAccessPolicyConfig) -> Self {
        Self {
            key_prefix: config.key_prefix.clone(),
            allowed_keys: config.allowed_keys.clone(),
            can_send: config.can_send,
            cross_agent: config.cross_agent,
        }
    }
}

/// Tool for listing known sessions.
pub struct SessionsListTool {
    metadata: Arc<SqliteSessionMetadata>,
    policy: Option<SessionAccessPolicy>,
}

impl SessionsListTool {
    pub fn new(metadata: Arc<SqliteSessionMetadata>) -> Self {
        Self {
            metadata,
            policy: None,
        }
    }

    /// Attach a session access policy for filtering.
    pub fn with_policy(mut self, policy: SessionAccessPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

/// Tool for reading history from a target session.
pub struct SessionsHistoryTool {
    store: Arc<SessionStore>,
    metadata: Arc<SqliteSessionMetadata>,
    policy: Option<SessionAccessPolicy>,
}

impl SessionsHistoryTool {
    pub fn new(store: Arc<SessionStore>, metadata: Arc<SqliteSessionMetadata>) -> Self {
        Self {
            store,
            metadata,
            policy: None,
        }
    }

    /// Attach a session access policy for filtering.
    pub fn with_policy(mut self, policy: SessionAccessPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

/// Tool for searching across session history.
pub struct SessionsSearchTool {
    store: Arc<SessionStore>,
    metadata: Arc<SqliteSessionMetadata>,
    policy: Option<SessionAccessPolicy>,
}

impl SessionsSearchTool {
    pub fn new(store: Arc<SessionStore>, metadata: Arc<SqliteSessionMetadata>) -> Self {
        Self {
            store,
            metadata,
            policy: None,
        }
    }

    /// Attach a session access policy for filtering.
    pub fn with_policy(mut self, policy: SessionAccessPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

/// Tool for sending a message to another session.
pub struct SessionsSendTool {
    metadata: Arc<SqliteSessionMetadata>,
    send_fn: SendToSessionFn,
    policy: Option<SessionAccessPolicy>,
}

impl SessionsSendTool {
    pub fn new(metadata: Arc<SqliteSessionMetadata>, send_fn: SendToSessionFn) -> Self {
        Self {
            metadata,
            send_fn,
            policy: None,
        }
    }

    /// Attach a session access policy for filtering.
    pub fn with_policy(mut self, policy: SessionAccessPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

#[async_trait]
impl AgentTool for SessionsListTool {
    fn name(&self) -> &str {
        "sessions_list"
    }

    fn description(&self) -> &str {
        "List available sessions with metadata. Supports optional text filtering and limit."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Optional substring to match against session key or label."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum sessions returned (default: 20, max: 100)."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let filter = str_param(&params, "filter").map(|v| v.to_lowercase());
        let limit = u64_param(&params, "limit", 20).min(100) as usize;

        let mut sessions: Vec<Value> = self
            .metadata
            .list()
            .await
            .into_iter()
            .filter(|entry| {
                // Apply session access policy.
                if let Some(ref policy) = self.policy
                    && !policy.can_access(&entry.key)
                {
                    return false;
                }
                filter.as_ref().is_none_or(|needle| {
                    let key_match = entry.key.to_lowercase().contains(needle);
                    let label_match = entry
                        .label
                        .as_ref()
                        .map(|label| label.to_lowercase().contains(needle))
                        .unwrap_or(false);
                    key_match || label_match
                })
            })
            .take(limit)
            .map(|entry| {
                serde_json::json!({
                    "id": entry.id,
                    "key": entry.key,
                    "label": entry.label,
                    "model": entry.model,
                    "messageCount": entry.message_count,
                    "createdAt": entry.created_at,
                    "updatedAt": entry.updated_at,
                    "projectId": entry.project_id,
                    "agentId": entry.agent_id,
                    "version": entry.version,
                })
            })
            .collect();
        let count = sessions.len();
        sessions.shrink_to_fit();

        Ok(serde_json::json!({
            "sessions": sessions,
            "count": count,
        }))
    }
}

#[async_trait]
impl AgentTool for SessionsSearchTool {
    fn name(&self) -> &str {
        "sessions_search"
    }

    fn description(&self) -> &str {
        "Search past conversations across sessions. Discovery mode (pass `query`) returns the \
         best-matching sessions with a snippet, surrounding messages, and how each session \
         started and ended. Scroll mode (pass `key` + `around_index`) pages through one \
         session around a previous match. Use sessions_history to read a full session."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query. Words are matched with BM25 ranking; use \"double quotes\" for exact phrases."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum sessions returned in discovery mode (default: 3, max: 10)."
                },
                "key": {
                    "type": "string",
                    "description": "Scroll mode: session key to page through (from a discovery result)."
                },
                "around_index": {
                    "type": "integer",
                    "description": "Scroll mode: message index to center the window on."
                },
                "window": {
                    "type": "integer",
                    "description": "Scroll mode: messages either side of around_index (default: 5, max: 20)."
                },
                "sort": {
                    "type": "string",
                    "enum": ["rank", "newest", "oldest"],
                    "description": "Result order in discovery mode (default: rank)."
                },
                "roles": {
                    "type": "array",
                    "items": {"type": "string", "enum": ["user", "assistant", "tool"]},
                    "description": "Roles to match (default: user and assistant)."
                },
                "exclude_current": {
                    "type": "boolean",
                    "description": "Exclude the current session from results. Defaults to true."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let scroll_key = str_param(&params, "key");
        let around_index = params.get("around_index").and_then(Value::as_i64);
        if let (Some(key), Some(around)) = (scroll_key, around_index) {
            return self.scroll(&params, key, around).await;
        }

        let query = require_str(&params, "query")?;
        match self.store.search_index() {
            Some(index) => self.discover(&params, query, index.clone()).await,
            None => self.legacy_search(&params, query).await,
        }
    }
}

impl SessionsSearchTool {
    /// Session-key prefixes demoted below interactive sessions in discovery.
    const DEMOTED_PREFIXES: &'static [&'static str] = &["cron:"];

    fn current_session_key(params: &Value) -> Option<String> {
        let exclude_current = params
            .get("exclude_current")
            .or_else(|| params.get("excludeCurrent"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        exclude_current
            .then(|| str_param(params, "_session_key").map(str::to_string))
            .flatten()
    }

    fn can_access(&self, key: &str) -> bool {
        self.policy
            .as_ref()
            .is_none_or(|policy| policy.can_access(key))
    }

    /// Discovery mode: FTS-ranked best hit per session with context.
    async fn discover(
        &self,
        params: &Value,
        query: &str,
        index: Arc<moltis_sessions::SessionSearchIndex>,
    ) -> anyhow::Result<Value> {
        let limit = u64_param(params, "limit", 3).clamp(1, 10) as usize;

        let mut search = moltis_sessions::SearchQuery::new(query);
        search.limit = limit;
        search.exclude_session = Self::current_session_key(params);
        if let Some(sort) = params.get("sort")
            && let Ok(sort) = serde_json::from_value(sort.clone())
        {
            search.sort = sort;
        }
        if let Some(roles) = params.get("roles").and_then(Value::as_array) {
            search.roles = roles
                .iter()
                .filter_map(|role| serde_json::from_value(role.clone()).ok())
                .collect();
        }

        let demote: Vec<String> = Self::DEMOTED_PREFIXES
            .iter()
            .map(ToString::to_string)
            .collect();
        let matches = index
            .discover(&search, &demote, 5)
            .await
            .map_err(|e| Error::message(format!("session search failed: {e}")))?;

        let entries: HashMap<String, moltis_sessions::metadata::SessionEntry> = self
            .metadata
            .list()
            .await
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();

        let results: Vec<Value> = matches
            .into_iter()
            .filter(|m| self.can_access(&m.session_key))
            .map(|m| {
                let entry = entries.get(&m.session_key);
                serde_json::json!({
                    "key": m.session_key,
                    "label": entry.and_then(|e| e.label.clone()),
                    "model": entry.and_then(|e| e.model.clone()),
                    "agentId": entry.and_then(|e| e.agent_id.clone()),
                    "updatedAt": entry.map(|e| e.updated_at),
                    "match": {
                        "index": m.hit.seq,
                        "role": m.hit.role,
                        "snippet": m.hit.snippet,
                        "compacted": m.hit.compacted,
                        "createdAt": m.hit.created_at,
                    },
                    "bookendStart": context_json(&m.bookend_start),
                    "context": context_json(&m.context),
                    "bookendEnd": context_json(&m.bookend_end),
                    "messagesBefore": m.messages_before,
                    "messagesAfter": m.messages_after,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "mode": "discovery",
            "query": query,
            "count": results.len(),
            "results": results,
            "hint": "Scroll with {key, around_index}; read a full session with sessions_history.",
        }))
    }

    /// Scroll mode: a window of messages around one index in one session.
    async fn scroll(&self, params: &Value, key: &str, around: i64) -> anyhow::Result<Value> {
        if !self.can_access(key) {
            return Err(Error::message(format!("session access denied: {key}")).into());
        }
        let index = self.store.search_index().ok_or_else(|| {
            Error::message("session search index is disabled; use sessions_history instead")
        })?;
        let window = u64_param(params, "window", 5).clamp(1, 20) as usize;
        let (messages, before, after) = index
            .context(key, around, window)
            .await
            .map_err(|e| Error::message(format!("failed to read session context: {e}")))?;
        Ok(serde_json::json!({
            "mode": "scroll",
            "key": key,
            "messages": context_json(&messages),
            "messagesBefore": before,
            "messagesAfter": after,
        }))
    }

    /// Fallback substring scan used when no search index is attached.
    async fn legacy_search(&self, params: &Value, query: &str) -> anyhow::Result<Value> {
        let limit = u64_param(params, "limit", 5).min(20) as usize;
        let current_session_key = Self::current_session_key(params);

        let entries: HashMap<String, moltis_sessions::metadata::SessionEntry> = self
            .metadata
            .list()
            .await
            .into_iter()
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        let known_keys = entries.keys().cloned().collect::<Vec<_>>();
        let search_limit = limit.saturating_mul(4).max(limit);
        let hits = self
            .store
            .search_known_keys(query, search_limit, &known_keys)
            .await
            .map_err(|e| Error::message(format!("failed to search sessions for '{query}': {e}")))?;

        let mut results = Vec::with_capacity(limit);
        for hit in hits {
            if results.len() >= limit {
                break;
            }

            if current_session_key.as_deref() == Some(hit.session_key.as_str()) {
                continue;
            }

            if !self.can_access(&hit.session_key) {
                continue;
            }

            let entry = entries.get(&hit.session_key);
            results.push(serde_json::json!({
                "key": hit.session_key,
                "label": entry.and_then(|value| value.label.clone()),
                "model": entry.and_then(|value| value.model.clone()),
                "projectId": entry.and_then(|value| value.project_id.clone()),
                "agentId": entry.and_then(|value| value.agent_id.clone()),
                "nodeId": entry.and_then(|value| value.node_id.clone()),
                "createdAt": entry.map(|value| value.created_at),
                "updatedAt": entry.map(|value| value.updated_at),
                "messageCount": entry.map(|value| value.message_count),
                "snippet": hit.snippet,
                "role": hit.role,
                "messageIndex": hit.message_index,
            }));
        }

        Ok(serde_json::json!({
            "query": query,
            "count": results.len(),
            "results": results,
        }))
    }
}

fn context_json(messages: &[moltis_sessions::search_index::ContextMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            serde_json::json!({
                "index": msg.seq,
                "role": msg.role,
                "content": msg.content,
                "createdAt": msg.created_at,
                "isMatch": msg.is_match,
            })
        })
        .collect()
}

#[async_trait]
impl AgentTool for SessionsHistoryTool {
    fn name(&self) -> &str {
        "sessions_history"
    }

    fn description(&self) -> &str {
        "Read paginated message history from another session."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Session key to read."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum messages to return (default: 20, max: 100)."
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip this many newest messages (default: 0)."
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let key = require_str(&params, "key")?;

        // Enforce session access policy.
        if let Some(ref policy) = self.policy
            && !policy.can_access(key)
        {
            return Err(Error::message(format!("session access denied: {key}")).into());
        }

        let limit = u64_param(&params, "limit", 20).min(100) as usize;
        let offset = u64_param(&params, "offset", 0) as usize;

        let entry = self
            .metadata
            .get(key)
            .await
            .ok_or_else(|| Error::message(format!("session not found: {key}")))?;
        let all_messages = self
            .store
            .read(key)
            .await
            .map_err(|e| Error::message(format!("failed to read session '{key}': {e}")))?;
        let total = all_messages.len();

        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        let messages: Vec<Value> = all_messages[start..end].to_vec();

        Ok(serde_json::json!({
            "key": key,
            "label": entry.label,
            "messages": messages,
            "totalMessages": total,
            "offset": offset,
            "count": end.saturating_sub(start),
            "hasMore": start > 0,
        }))
    }
}

#[async_trait]
impl AgentTool for SessionsSendTool {
    fn name(&self) -> &str {
        "sessions_send"
    }

    fn description(&self) -> &str {
        "Send a message to another session. Optionally wait for the target session's reply."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Session key to send to."
                },
                "message": {
                    "type": "string",
                    "description": "Message text to send."
                },
                "wait_for_reply": {
                    "type": "boolean",
                    "description": "Wait for a synchronous response from the target session."
                },
                "context": {
                    "type": "string",
                    "description": "Optional sender context prepended to the message."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override for the target session turn."
                }
            },
            "required": ["key", "message"]
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let key = require_str(&params, "key")?.to_string();
        let message = require_str(&params, "message")?.to_string();
        let wait_for_reply = bool_param(&params, "wait_for_reply", false)
            || bool_param(&params, "waitForReply", false);
        let context = owned_str_param(&params, &["context"]);
        let model = owned_str_param(&params, &["model"]);

        // Enforce session access policy.
        if let Some(ref policy) = self.policy {
            if !policy.can_access(&key) {
                return Err(Error::message(format!("session access denied: {key}")).into());
            }
            if !policy.can_send {
                return Err(
                    Error::message("session policy denies sending messages".to_string()).into(),
                );
            }
        }

        let entry = self
            .metadata
            .get(&key)
            .await
            .ok_or_else(|| Error::message(format!("session not found: {key}")))?;

        let message = if let Some(ctx) = context {
            format!("[From: {ctx}]\n\n{message}")
        } else {
            message
        };

        let result = (self.send_fn)(SendToSessionRequest {
            key: key.clone(),
            message,
            wait_for_reply,
            model,
        })
        .await?;

        Ok(serde_json::json!({
            "key": key,
            "label": entry.label,
            "sent": true,
            "waitForReply": wait_for_reply,
            "result": result,
        }))
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

    async fn test_pool() -> TestResult<sqlx::SqlitePool> {
        let pool = sqlx::SqlitePool::connect(":memory:").await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS projects (id TEXT PRIMARY KEY)")
            .execute(&pool)
            .await?;
        SqliteSessionMetadata::init(&pool).await?;
        Ok(pool)
    }

    #[tokio::test]
    async fn sessions_list_filters_and_limits() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata.upsert("main", Some("Main".to_string())).await?;
        metadata
            .upsert("session:alpha", Some("Alpha".to_string()))
            .await?;
        metadata
            .upsert("session:beta", Some("Beta".to_string()))
            .await?;

        let tool = SessionsListTool::new(metadata);
        let result = tool
            .execute(serde_json::json!({
                "filter": "alp",
                "limit": 5
            }))
            .await?;

        assert_eq!(result["count"], 1);
        let sessions = result
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| std::io::Error::other("missing sessions array"))?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["key"], "session:alpha");
        Ok(())
    }

    #[tokio::test]
    async fn sessions_history_reads_paginated_messages() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("session:history", Some("History".to_string()))
            .await?;

        let tmp = tempfile::tempdir()?;
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        store
            .append(
                "session:history",
                &serde_json::json!({
                    "role": "user",
                    "content": "one"
                }),
            )
            .await?;
        store
            .append(
                "session:history",
                &serde_json::json!({
                    "role": "assistant",
                    "content": "two"
                }),
            )
            .await?;
        store
            .append(
                "session:history",
                &serde_json::json!({
                    "role": "user",
                    "content": "three"
                }),
            )
            .await?;

        let tool = SessionsHistoryTool::new(store, metadata);
        let result = tool
            .execute(serde_json::json!({
                "key": "session:history",
                "limit": 2
            }))
            .await?;

        assert_eq!(result["totalMessages"], 3);
        assert_eq!(result["count"], 2);
        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| std::io::Error::other("missing messages array"))?;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "two");
        assert_eq!(messages[1]["content"], "three");
        Ok(())
    }

    #[tokio::test]
    async fn sessions_history_rejects_missing_session() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        let tmp = tempfile::tempdir()?;
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        let tool = SessionsHistoryTool::new(store, metadata);

        let result = tool
            .execute(serde_json::json!({
                "key": "session:missing"
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected missing session error"))?;
        assert!(err.to_string().contains("session not found"));
        Ok(())
    }

    #[tokio::test]
    async fn sessions_search_finds_matches_and_excludes_current_by_default() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("session:current", Some("Current".to_string()))
            .await?;
        metadata
            .upsert("session:other", Some("Other".to_string()))
            .await?;

        let tmp = tempfile::tempdir()?;
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        store
            .append(
                "session:current",
                &serde_json::json!({
                    "role": "user",
                    "content": "rust checkpoint design"
                }),
            )
            .await?;
        store
            .append(
                "session:other",
                &serde_json::json!({
                    "role": "assistant",
                    "content": "rust checkpoint design with rollback"
                }),
            )
            .await?;

        let tool = SessionsSearchTool::new(store, metadata);
        let result = tool
            .execute(serde_json::json!({
                "query": "checkpoint",
                "_session_key": "session:current"
            }))
            .await?;

        assert_eq!(result["count"], 1);
        let results = result
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| std::io::Error::other("missing results array"))?;
        assert_eq!(results[0]["key"], "session:other");
        Ok(())
    }

    #[tokio::test]
    async fn sessions_search_can_include_current_session() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("session:current", Some("Current".to_string()))
            .await?;

        let tmp = tempfile::tempdir()?;
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        store
            .append(
                "session:current",
                &serde_json::json!({
                    "role": "user",
                    "content": "needle in current session"
                }),
            )
            .await?;

        let tool = SessionsSearchTool::new(store, metadata);
        let result = tool
            .execute(serde_json::json!({
                "query": "needle",
                "_session_key": "session:current",
                "exclude_current": false
            }))
            .await?;

        assert_eq!(result["count"], 1);
        assert_eq!(result["results"][0]["key"], "session:current");
        Ok(())
    }

    #[tokio::test]
    async fn sessions_send_calls_callback_and_wraps_context() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("session:target", Some("Target".to_string()))
            .await?;

        let called = Arc::new(AtomicBool::new(false));
        let called_ref = Arc::clone(&called);
        let send_fn: SendToSessionFn = Arc::new(move |req| {
            let called_ref = Arc::clone(&called_ref);
            Box::pin(async move {
                called_ref.store(true, Ordering::SeqCst);
                assert_eq!(req.key, "session:target");
                assert!(req.message.starts_with("[From: coordinator]"));
                assert!(req.wait_for_reply);
                Ok(serde_json::json!({
                    "text": "ok",
                    "inputTokens": 1,
                    "outputTokens": 1
                }))
            })
        });
        let tool = SessionsSendTool::new(metadata, send_fn);

        let result = tool
            .execute(serde_json::json!({
                "key": "session:target",
                "message": "Do work",
                "context": "coordinator",
                "wait_for_reply": true
            }))
            .await?;

        assert_eq!(result["sent"], true);
        assert_eq!(result["waitForReply"], true);
        assert_eq!(result["result"]["text"], "ok");
        assert!(called.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn sessions_send_rejects_missing_target() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        let send_fn: SendToSessionFn = Arc::new(move |_req| {
            Box::pin(async move {
                Ok(serde_json::json!({
                    "ok": true
                }))
            })
        });
        let tool = SessionsSendTool::new(metadata, send_fn);

        let result = tool
            .execute(serde_json::json!({
                "key": "session:missing",
                "message": "hello"
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected missing target error"))?;
        assert!(err.to_string().contains("session not found"));
        Ok(())
    }

    #[tokio::test]
    async fn test_list_filtered_by_key_prefix() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("agent:scout:1", Some("Scout 1".into()))
            .await?;
        metadata
            .upsert("agent:scout:2", Some("Scout 2".into()))
            .await?;
        metadata
            .upsert("agent:coder:1", Some("Coder 1".into()))
            .await?;

        let policy = SessionAccessPolicy {
            key_prefix: Some("agent:scout:".into()),
            ..Default::default()
        };
        let tool = SessionsListTool::new(metadata).with_policy(policy);
        let result = tool.execute(serde_json::json!({})).await?;

        assert_eq!(result["count"], 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_search_filtered_by_key_prefix() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("agent:scout:1", Some("Scout 1".into()))
            .await?;
        metadata
            .upsert("agent:coder:1", Some("Coder 1".into()))
            .await?;

        let tmp = tempfile::tempdir()?;
        let store = Arc::new(SessionStore::new(tmp.path().to_path_buf()));
        store
            .append(
                "agent:scout:1",
                &serde_json::json!({"role": "user", "content": "shared search term"}),
            )
            .await?;
        store
            .append(
                "agent:coder:1",
                &serde_json::json!({"role": "user", "content": "shared search term"}),
            )
            .await?;

        let policy = SessionAccessPolicy {
            key_prefix: Some("agent:scout:".into()),
            ..Default::default()
        };
        let tool = SessionsSearchTool::new(store, metadata).with_policy(policy);
        let result = tool.execute(serde_json::json!({"query": "shared"})).await?;

        assert_eq!(result["count"], 1);
        assert_eq!(result["results"][0]["key"], "agent:scout:1");
        Ok(())
    }

    #[tokio::test]
    async fn test_send_denied_when_can_send_false() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("session:target", Some("Target".into()))
            .await?;

        let send_fn: SendToSessionFn =
            Arc::new(move |_req| Box::pin(async move { Ok(serde_json::json!({"ok": true})) }));
        let policy = SessionAccessPolicy {
            can_send: false,
            ..Default::default()
        };
        let tool = SessionsSendTool::new(metadata, send_fn).with_policy(policy);

        let result = tool
            .execute(serde_json::json!({
                "key": "session:target",
                "message": "hello"
            }))
            .await;

        let err = result.expect_err("should deny sending");
        assert!(err.to_string().contains("denies sending"));
        Ok(())
    }

    #[tokio::test]
    async fn test_no_policy_allows_all() -> TestResult<()> {
        let metadata = Arc::new(SqliteSessionMetadata::new(test_pool().await?));
        metadata
            .upsert("agent:scout:1", Some("Scout 1".into()))
            .await?;
        metadata
            .upsert("agent:coder:1", Some("Coder 1".into()))
            .await?;

        // No policy = all sessions visible.
        let tool = SessionsListTool::new(metadata);
        let result = tool.execute(serde_json::json!({})).await?;

        assert_eq!(result["count"], 2);
        Ok(())
    }

    // ── indexed search (discovery + scroll) ─────────────────────

    async fn indexed_store(
        dir: &tempfile::TempDir,
    ) -> TestResult<(Arc<SessionStore>, Arc<SqliteSessionMetadata>)> {
        let pool = sqlx::SqlitePool::connect(":memory:").await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS projects (id TEXT PRIMARY KEY)")
            .execute(&pool)
            .await?;
        moltis_sessions::run_migrations(&pool).await?;
        SqliteSessionMetadata::init(&pool).await?;
        let index = Arc::new(moltis_sessions::SessionSearchIndex::new(pool.clone()));
        let store = Arc::new(SessionStore::new(dir.path().to_path_buf()).with_search_index(index));
        let metadata = Arc::new(SqliteSessionMetadata::new(pool));
        Ok((store, metadata))
    }

    #[tokio::test]
    async fn indexed_discovery_returns_context_and_bookends() -> TestResult<()> {
        let dir = tempfile::tempdir()?;
        let (store, metadata) = indexed_store(&dir).await?;
        metadata
            .upsert("session:old", Some("Old work".to_string()))
            .await?;
        for i in 0..5 {
            store
                .append(
                    "session:old",
                    &serde_json::json!({"role": "user", "content": format!("filler {i}")}),
                )
                .await?;
        }
        store
            .append(
                "session:old",
                &serde_json::json!({"role": "user", "content": "we fixed the flaky websocket test"}),
            )
            .await?;
        store
            .append(
                "session:old",
                &serde_json::json!({"role": "assistant", "content": "confirmed fix landed"}),
            )
            .await?;

        let tool = SessionsSearchTool::new(store, metadata);
        let result = tool
            .execute(serde_json::json!({"query": "flaky websocket"}))
            .await?;

        assert_eq!(result["mode"], "discovery");
        assert_eq!(result["count"], 1);
        let hit = &result["results"][0];
        assert_eq!(hit["key"], "session:old");
        assert_eq!(hit["label"], "Old work");
        assert!(
            hit["match"]["snippet"]
                .as_str()
                .unwrap_or_default()
                .contains(">>>flaky<<<")
        );
        assert_eq!(hit["match"]["index"], 5);
        assert!(!hit["bookendStart"].as_array().unwrap_or(&vec![]).is_empty());
        assert!(
            hit["context"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .any(|m| m["isMatch"] == true)
        );
        Ok(())
    }

    #[tokio::test]
    async fn indexed_discovery_excludes_current_session() -> TestResult<()> {
        let dir = tempfile::tempdir()?;
        let (store, metadata) = indexed_store(&dir).await?;
        store
            .append(
                "current",
                &serde_json::json!({"role": "user", "content": "shared marker phrase"}),
            )
            .await?;
        store
            .append(
                "other",
                &serde_json::json!({"role": "user", "content": "shared marker phrase"}),
            )
            .await?;

        let tool = SessionsSearchTool::new(store, metadata);
        let result = tool
            .execute(serde_json::json!({
                "query": "shared marker",
                "_session_key": "current",
            }))
            .await?;

        assert_eq!(result["count"], 1);
        assert_eq!(result["results"][0]["key"], "other");
        Ok(())
    }

    #[tokio::test]
    async fn indexed_scroll_pages_around_index() -> TestResult<()> {
        let dir = tempfile::tempdir()?;
        let (store, metadata) = indexed_store(&dir).await?;
        for i in 0..20 {
            store
                .append(
                    "s1",
                    &serde_json::json!({"role": "user", "content": format!("message {i}")}),
                )
                .await?;
        }

        let tool = SessionsSearchTool::new(store, metadata);
        let result = tool
            .execute(serde_json::json!({"key": "s1", "around_index": 10, "window": 2}))
            .await?;

        assert_eq!(result["mode"], "scroll");
        let messages = result["messages"].as_array().cloned().unwrap_or_default();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0]["index"], 8);
        assert_eq!(messages[4]["index"], 12);
        assert_eq!(result["messagesBefore"], 8);
        assert_eq!(result["messagesAfter"], 7);
        Ok(())
    }

    #[tokio::test]
    async fn indexed_search_respects_access_policy() -> TestResult<()> {
        let dir = tempfile::tempdir()?;
        let (store, metadata) = indexed_store(&dir).await?;
        store
            .append(
                "agent:private:1",
                &serde_json::json!({"role": "user", "content": "restricted content"}),
            )
            .await?;

        let policy = SessionAccessPolicy {
            key_prefix: Some("agent:public:".to_string()),
            ..Default::default()
        };
        let tool = SessionsSearchTool::new(store, metadata).with_policy(policy);
        let result = tool
            .execute(serde_json::json!({"query": "restricted content"}))
            .await?;
        assert_eq!(result["count"], 0);

        let scroll = tool
            .execute(serde_json::json!({"key": "agent:private:1", "around_index": 0}))
            .await;
        assert!(scroll.is_err());
        Ok(())
    }
}
