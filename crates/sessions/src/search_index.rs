//! FTS5-backed search index over session transcripts.
//!
//! JSONL files remain the transcript source of truth ([`crate::store::SessionStore`]);
//! this module maintains a derived SQLite projection (`session_messages` +
//! `session_messages_fts`) used for cross-session recall. Rows removed from the
//! live transcript by compaction are kept with `compacted = 1` so summarized-away
//! content stays searchable.
//!
//! Indexing is incremental and idempotent: each session tracks a monotone `seq`,
//! and backfill catches a session up by comparing indexed rows against JSONL
//! lines, so a crash mid-backfill only means re-checking counts on the next run.

use {
    serde::{Deserialize, Serialize},
    sqlx::{QueryBuilder, Row, SqlitePool},
};

use crate::Result;

/// Maximum raw query length accepted by the sanitizer.
const MAX_QUERY_CHARS: usize = 1024;
/// How many raw FTS hits a discovery scan considers before grouping by session.
const DISCOVER_SCAN_LIMIT: usize = 300;
/// Messages inserted per transaction during backfill.
const BACKFILL_CHUNK: usize = 500;
/// Bookend size (first/last conversational messages of a session).
const BOOKEND_MESSAGES: usize = 3;

/// Sort order for flat search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchSort {
    /// BM25 relevance (best match first).
    #[default]
    Rank,
    Newest,
    Oldest,
}

/// Message roles stored in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexedRole {
    User,
    Assistant,
    Tool,
}

impl IndexedRole {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }

    fn from_role_str(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "tool" | "tool_result" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// A message extracted from a transcript line for indexing.
#[derive(Debug, Clone)]
pub struct IndexableMessage {
    pub role: IndexedRole,
    pub content: String,
    pub tool_name: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at: u64,
}

impl IndexableMessage {
    /// Extract the indexable text of a persisted JSONL message.
    ///
    /// Returns `None` for roles that should not be indexed (system prompts,
    /// UI notices) and for messages without any text content.
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        let role = IndexedRole::from_role_str(value.get("role")?.as_str()?)?;
        let content = extract_text(value.get("content")?)?;
        if content.trim().is_empty() {
            return None;
        }
        let tool_name = value
            .get("tool_name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let created_at = value
            .get("created_at")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(crate::message::now_ms);
        Some(Self {
            role,
            content,
            tool_name,
            created_at,
        })
    }
}

fn extract_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter_map(|block| {
            (block.get("type")?.as_str()? == "text")
                .then(|| block.get("text")?.as_str())
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// A single FTS hit.
#[derive(Debug, Clone, Serialize)]
pub struct MessageHit {
    pub session_key: String,
    pub seq: i64,
    pub role: String,
    pub snippet: String,
    /// Milliseconds since the Unix epoch.
    pub created_at: u64,
    /// True when this row was archived by compaction (no longer in live context).
    pub compacted: bool,
}

/// A message returned in context windows and bookends.
#[derive(Debug, Clone, Serialize)]
pub struct ContextMessage {
    pub seq: i64,
    pub role: String,
    pub content: String,
    /// Milliseconds since the Unix epoch.
    pub created_at: u64,
    /// True when this is the message the search matched.
    pub is_match: bool,
}

/// A discovery result: the best hit in one session plus surrounding context.
#[derive(Debug, Clone, Serialize)]
pub struct SessionMatch {
    pub session_key: String,
    pub hit: MessageHit,
    /// First conversational messages of the session ("what was the goal").
    pub bookend_start: Vec<ContextMessage>,
    /// Messages around the match.
    pub context: Vec<ContextMessage>,
    /// Last conversational messages of the session ("how did it end").
    pub bookend_end: Vec<ContextMessage>,
    pub messages_before: i64,
    pub messages_after: i64,
}

/// Parameters for a flat search.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub query: String,
    pub limit: usize,
    pub sort: SearchSort,
    /// Restrict hits to these roles; empty means user + assistant.
    pub roles: Vec<IndexedRole>,
    /// Session key to exclude (usually the caller's own session).
    pub exclude_session: Option<String>,
}

impl SearchQuery {
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: 20,
            sort: SearchSort::Rank,
            roles: vec![],
            exclude_session: None,
        }
    }

    fn effective_roles(&self) -> Vec<IndexedRole> {
        if self.roles.is_empty() {
            vec![IndexedRole::User, IndexedRole::Assistant]
        } else {
            self.roles.clone()
        }
    }
}

/// Sanitize a user query into a safe FTS5 MATCH expression.
///
/// Every whitespace-separated token becomes a quoted phrase (embedded quotes
/// doubled), which sidesteps the entire FTS5 operator grammar: `AND`/`OR`/
/// `NOT`/`NEAR`, column filters, parens, and wildcards are all treated as
/// literal text. User-supplied double-quoted phrases are preserved as phrases.
/// Returns `None` when nothing searchable remains.
#[must_use]
pub fn sanitize_fts_query(raw: &str) -> Option<String> {
    let bounded: String = raw.chars().take(MAX_QUERY_CHARS).collect();
    let mut terms: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    let mut push_term = |term: &mut String| {
        let cleaned: String = term
            .chars()
            .filter(|c| c.is_alphanumeric() || " .:@/\\-_'".contains(*c))
            .collect();
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            terms.push(format!("\"{}\"", trimmed.replace('"', "\"\"")));
        }
        term.clear();
    };

    for c in bounded.chars() {
        match c {
            '"' => {
                push_term(&mut current);
                in_quotes = !in_quotes;
            },
            c if c.is_whitespace() && !in_quotes => push_term(&mut current),
            c => current.push(c),
        }
    }
    push_term(&mut current);

    (!terms.is_empty()).then(|| terms.join(" "))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}…")
}

/// FTS5-backed index over session transcripts.
#[derive(Clone)]
pub struct SessionSearchIndex {
    pool: SqlitePool,
}

impl SessionSearchIndex {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Index one appended transcript message. No-op for non-indexable values.
    #[tracing::instrument(skip_all, fields(session_key = %key))]
    pub async fn index_appended(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        let Some(message) = IndexableMessage::from_value(value) else {
            return Ok(());
        };
        sqlx::query(
            "INSERT INTO session_messages (session_key, seq, role, content, tool_name, created_at)
             SELECT ?1, COALESCE(MAX(seq) + 1, 0), ?2, ?3, ?4, ?5
             FROM session_messages WHERE session_key = ?1",
        )
        .bind(key)
        .bind(message.role.as_str())
        .bind(&message.content)
        .bind(&message.tool_name)
        .bind(message.created_at as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reflect a history replacement (compaction): archive current rows as
    /// `compacted`, then index the replacement messages as fresh rows.
    ///
    /// Archived rows stay in the FTS index, so content summarized out of the
    /// live context window remains discoverable.
    #[tracing::instrument(skip_all, fields(session_key = %key))]
    pub async fn index_replacement(&self, key: &str, messages: &[serde_json::Value]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE session_messages SET active = 0, compacted = 1
             WHERE session_key = ? AND active = 1",
        )
        .bind(key)
        .execute(&mut *tx)
        .await?;
        let mut next_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM session_messages WHERE session_key = ?",
        )
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
        for value in messages {
            let Some(message) = IndexableMessage::from_value(value) else {
                continue;
            };
            sqlx::query(
                "INSERT INTO session_messages (session_key, seq, role, content, tool_name, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(key)
            .bind(next_seq)
            .bind(message.role.as_str())
            .bind(&message.content)
            .bind(&message.tool_name)
            .bind(message.created_at as i64)
            .execute(&mut *tx)
            .await?;
            next_seq += 1;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Remove a session from the index entirely (session deleted).
    pub async fn remove_session(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM session_messages WHERE session_key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Flat BM25 search over indexed messages.
    #[tracing::instrument(skip_all)]
    pub async fn search(&self, params: &SearchQuery) -> Result<Vec<MessageHit>> {
        let Some(match_expr) = sanitize_fts_query(&params.query) else {
            return Ok(vec![]);
        };
        let roles = params.effective_roles();

        let mut builder = QueryBuilder::new(
            "SELECT m.session_key, m.seq, m.role, m.created_at, m.compacted, \
             snippet(session_messages_fts, 0, '>>>', '<<<', ' … ', 40) AS snip \
             FROM session_messages_fts \
             JOIN session_messages m ON m.id = session_messages_fts.rowid \
             WHERE session_messages_fts MATCH ",
        );
        builder.push_bind(&match_expr);
        builder.push(" AND (m.active = 1 OR m.compacted = 1) AND m.role IN (");
        {
            let mut separated = builder.separated(", ");
            for role in &roles {
                separated.push_bind(role.as_str());
            }
        }
        builder.push(")");
        if let Some(exclude) = &params.exclude_session {
            builder.push(" AND m.session_key != ");
            builder.push_bind(exclude);
        }
        match params.sort {
            SearchSort::Rank => builder.push(" ORDER BY rank"),
            SearchSort::Newest => builder.push(" ORDER BY m.created_at DESC, rank"),
            SearchSort::Oldest => builder.push(" ORDER BY m.created_at ASC, rank"),
        };
        builder.push(" LIMIT ");
        builder.push_bind(params.limit as i64);

        let rows = builder.build().fetch_all(&self.pool).await?;
        Ok(rows.iter().map(hit_from_row).collect())
    }

    /// Discovery search: the best hit per session, with context and bookends.
    ///
    /// Sessions whose key starts with a prefix in `demote_prefixes` (e.g.
    /// background cron sessions) are stably sorted below interactive ones.
    #[tracing::instrument(skip_all)]
    pub async fn discover(
        &self,
        params: &SearchQuery,
        demote_prefixes: &[String],
        context_window: usize,
    ) -> Result<Vec<SessionMatch>> {
        let scan = SearchQuery {
            limit: DISCOVER_SCAN_LIMIT,
            ..params.clone()
        };
        let hits = self.search(&scan).await?;

        // Best (first-seen, rank-ordered) hit per session.
        let mut best: Vec<MessageHit> = Vec::new();
        for hit in hits {
            if !best.iter().any(|b| b.session_key == hit.session_key) {
                best.push(hit);
            }
        }
        // Stable demotion keeps rank order within each class.
        best.sort_by_key(|hit| {
            demote_prefixes
                .iter()
                .any(|prefix| hit.session_key.starts_with(prefix.as_str()))
        });
        best.truncate(params.limit);

        let mut matches = Vec::with_capacity(best.len());
        for hit in best {
            let (context, before, after) = self
                .context(&hit.session_key, hit.seq, context_window)
                .await?;
            let (bookend_start, bookend_end) = self.bookends(&hit.session_key).await?;
            matches.push(SessionMatch {
                session_key: hit.session_key.clone(),
                context: context
                    .into_iter()
                    .map(|mut msg| {
                        msg.is_match = msg.seq == hit.seq;
                        msg
                    })
                    .collect(),
                bookend_start,
                bookend_end,
                messages_before: before,
                messages_after: after,
                hit,
            });
        }
        Ok(matches)
    }

    /// Messages around `seq` in one session, plus counts outside the window.
    pub async fn context(
        &self,
        key: &str,
        seq: i64,
        window: usize,
    ) -> Result<(Vec<ContextMessage>, i64, i64)> {
        let window = window as i64;
        let low = seq.saturating_sub(window);
        let high = seq.saturating_add(window);
        let rows = sqlx::query(
            "SELECT seq, role, content, created_at FROM session_messages
             WHERE session_key = ? AND seq BETWEEN ? AND ?
               AND (active = 1 OR compacted = 1)
             ORDER BY seq ASC",
        )
        .bind(key)
        .bind(low)
        .bind(high)
        .fetch_all(&self.pool)
        .await?;
        let messages = rows.iter().map(|row| context_from_row(row, 4000)).collect();

        let before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_messages
             WHERE session_key = ? AND seq < ? AND (active = 1 OR compacted = 1)",
        )
        .bind(key)
        .bind(low)
        .fetch_one(&self.pool)
        .await?;
        let after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_messages
             WHERE session_key = ? AND seq > ? AND (active = 1 OR compacted = 1)",
        )
        .bind(key)
        .bind(high)
        .fetch_one(&self.pool)
        .await?;
        Ok((messages, before, after))
    }

    /// First and last conversational (user/assistant) messages of a session.
    pub async fn bookends(&self, key: &str) -> Result<(Vec<ContextMessage>, Vec<ContextMessage>)> {
        let first = sqlx::query(
            "SELECT seq, role, content, created_at FROM session_messages
             WHERE session_key = ? AND role IN ('user', 'assistant')
               AND (active = 1 OR compacted = 1)
             ORDER BY seq ASC LIMIT ?",
        )
        .bind(key)
        .bind(BOOKEND_MESSAGES as i64)
        .fetch_all(&self.pool)
        .await?;
        let mut last = sqlx::query(
            "SELECT seq, role, content, created_at FROM session_messages
             WHERE session_key = ? AND role IN ('user', 'assistant')
               AND (active = 1 OR compacted = 1)
             ORDER BY seq DESC LIMIT ?",
        )
        .bind(key)
        .bind(BOOKEND_MESSAGES as i64)
        .fetch_all(&self.pool)
        .await?;
        last.reverse();

        let start: Vec<ContextMessage> = first
            .iter()
            .map(|row| context_from_row(row, 1200))
            .collect();
        let end: Vec<ContextMessage> = last
            .iter()
            .map(|row| context_from_row(row, 1200))
            // Drop overlap with the start bookend in short sessions.
            .filter(|msg| !start.iter().any(|s| s.seq == msg.seq))
            .collect();
        Ok((start, end))
    }

    /// Catch one session's index up with its JSONL transcript.
    ///
    /// Idempotent: only lines beyond the highest indexed `seq` are inserted,
    /// in chunked transactions, so interrupted backfills resume safely.
    /// Returns the number of newly indexed messages.
    #[tracing::instrument(skip_all, fields(session_key = %key))]
    pub async fn sync_session(&self, store: &crate::store::SessionStore, key: &str) -> Result<u64> {
        let indexed: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM session_messages WHERE session_key = ?",
        )
        .bind(key)
        .fetch_one(&self.pool)
        .await?;
        let messages = store.read(key).await?;
        if messages.len() as i64 <= indexed {
            return Ok(0);
        }

        let mut inserted = 0u64;
        let pending: Vec<(i64, IndexableMessage)> = messages
            .iter()
            .enumerate()
            .skip(indexed as usize)
            .filter_map(|(idx, value)| {
                IndexableMessage::from_value(value).map(|msg| (idx as i64, msg))
            })
            .collect();
        for chunk in pending.chunks(BACKFILL_CHUNK) {
            let mut tx = self.pool.begin().await?;
            for (seq, message) in chunk {
                let result = sqlx::query(
                    "INSERT OR IGNORE INTO session_messages
                     (session_key, seq, role, content, tool_name, created_at)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(key)
                .bind(seq)
                .bind(message.role.as_str())
                .bind(&message.content)
                .bind(&message.tool_name)
                .bind(message.created_at as i64)
                .execute(&mut *tx)
                .await?;
                inserted += result.rows_affected();
            }
            tx.commit().await?;
        }
        Ok(inserted)
    }

    /// Backfill the index from every stored session. Returns messages indexed.
    #[tracing::instrument(skip_all)]
    pub async fn sync_all(&self, store: &crate::store::SessionStore) -> Result<u64> {
        let mut total = 0u64;
        for key in store.list_keys() {
            match self.sync_session(store, &key).await {
                Ok(count) => total += count,
                Err(error) => {
                    tracing::warn!(session_key = %key, %error, "session index backfill failed");
                },
            }
        }
        #[cfg(feature = "metrics")]
        moltis_metrics::counter!("sessions_search_index_backfilled_total").increment(total);
        Ok(total)
    }

    /// Number of indexed messages across all sessions.
    pub async fn indexed_messages(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM session_messages")
            .fetch_one(&self.pool)
            .await?)
    }
}

fn hit_from_row(row: &sqlx::sqlite::SqliteRow) -> MessageHit {
    MessageHit {
        session_key: row.get("session_key"),
        seq: row.get("seq"),
        role: row.get("role"),
        snippet: row.get("snip"),
        created_at: row.get::<i64, _>("created_at").max(0) as u64,
        compacted: row.get::<i64, _>("compacted") != 0,
    }
}

fn context_from_row(row: &sqlx::sqlite::SqliteRow, max_chars: usize) -> ContextMessage {
    ContextMessage {
        seq: row.get("seq"),
        role: row.get("role"),
        content: truncate_chars(row.get("content"), max_chars),
        created_at: row.get::<i64, _>("created_at").max(0) as u64,
        is_match: false,
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, serde_json::json};

    async fn temp_index() -> SessionSearchIndex {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::run_migrations(&pool).await.unwrap();
        SessionSearchIndex::new(pool)
    }

    fn user_msg(text: &str) -> serde_json::Value {
        json!({"role": "user", "content": text, "created_at": 1000})
    }

    fn assistant_msg(text: &str) -> serde_json::Value {
        json!({"role": "assistant", "content": text, "created_at": 2000})
    }

    // ── sanitizer ───────────────────────────────────────────────

    #[test]
    fn sanitizer_quotes_plain_terms() {
        assert_eq!(
            sanitize_fts_query("hello world"),
            Some("\"hello\" \"world\"".into())
        );
    }

    #[test]
    fn sanitizer_neutralizes_fts_operators() {
        assert_eq!(
            sanitize_fts_query("a AND b OR NOT (c)"),
            Some("\"a\" \"AND\" \"b\" \"OR\" \"NOT\" \"c\"".into())
        );
        assert_eq!(
            sanitize_fts_query("col:value"),
            Some("\"col:value\"".into())
        );
        assert_eq!(sanitize_fts_query("wild*"), Some("\"wild\"".into()));
    }

    #[test]
    fn sanitizer_preserves_quoted_phrases() {
        assert_eq!(
            sanitize_fts_query("\"exact phrase\" extra"),
            Some("\"exact phrase\" \"extra\"".into())
        );
    }

    #[test]
    fn sanitizer_keeps_dotted_and_hyphenated_terms() {
        assert_eq!(
            sanitize_fts_query("chat-send helpers.ts"),
            Some("\"chat-send\" \"helpers.ts\"".into())
        );
    }

    #[test]
    fn sanitizer_rejects_empty_and_symbol_only() {
        assert_eq!(sanitize_fts_query(""), None);
        assert_eq!(sanitize_fts_query("   "), None);
        assert_eq!(sanitize_fts_query("^{}()*"), None);
    }

    #[test]
    fn sanitizer_bounds_query_length() {
        let long = "a ".repeat(MAX_QUERY_CHARS);
        let sanitized = sanitize_fts_query(&long).unwrap();
        assert!(sanitized.len() < MAX_QUERY_CHARS * 4);
    }

    // ── extraction ──────────────────────────────────────────────

    #[test]
    fn extracts_multimodal_text_blocks() {
        let value = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "part one"},
                {"type": "image_url", "image_url": {"url": "data:..."}},
                {"type": "text", "text": "part two"},
            ],
        });
        let msg = IndexableMessage::from_value(&value).unwrap();
        assert_eq!(msg.content, "part one\npart two");
    }

    #[test]
    fn skips_system_notice_and_empty() {
        assert!(IndexableMessage::from_value(&json!({"role": "system", "content": "x"})).is_none());
        assert!(IndexableMessage::from_value(&json!({"role": "notice", "content": "x"})).is_none());
        assert!(IndexableMessage::from_value(&json!({"role": "user", "content": "  "})).is_none());
    }

    // ── indexing + search ───────────────────────────────────────

    #[tokio::test]
    async fn index_and_search_roundtrip() {
        let index = temp_index().await;
        index
            .index_appended("s1", &user_msg("the mitochondria is the powerhouse"))
            .await
            .unwrap();
        index
            .index_appended("s2", &user_msg("unrelated content"))
            .await
            .unwrap();

        let hits = index
            .search(&SearchQuery::new("mitochondria powerhouse"))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_key, "s1");
        assert!(hits[0].snippet.contains(">>>mitochondria<<<"));
        assert!(!hits[0].compacted);
    }

    #[tokio::test]
    async fn search_excludes_current_session_and_filters_roles() {
        let index = temp_index().await;
        index
            .index_appended("current", &user_msg("shared topic"))
            .await
            .unwrap();
        index
            .index_appended("other", &assistant_msg("shared topic"))
            .await
            .unwrap();
        index
            .index_appended(
                "tools",
                &json!({"role": "tool", "content": "shared topic", "tool_name": "exec"}),
            )
            .await
            .unwrap();

        let mut params = SearchQuery::new("shared topic");
        params.exclude_session = Some("current".into());
        let hits = index.search(&params).await.unwrap();
        // Tool rows are excluded by the default role filter.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_key, "other");

        params.roles = vec![IndexedRole::Tool];
        let tool_hits = index.search(&params).await.unwrap();
        assert_eq!(tool_hits.len(), 1);
        assert_eq!(tool_hits[0].session_key, "tools");
    }

    #[tokio::test]
    async fn malicious_queries_do_not_error() {
        let index = temp_index().await;
        index
            .index_appended("s1", &user_msg("hello world"))
            .await
            .unwrap();
        for query in [
            "\"unbalanced",
            "a OR",
            "NEAR(a b)",
            "content:hello",
            "{malformed}",
            "-- ; DROP TABLE session_messages",
        ] {
            let result = index.search(&SearchQuery::new(query)).await;
            assert!(result.is_ok(), "query {query:?} errored: {result:?}");
        }
    }

    #[tokio::test]
    async fn replacement_archives_but_keeps_searchable() {
        let index = temp_index().await;
        index
            .index_appended("s1", &user_msg("original secret plan"))
            .await
            .unwrap();
        index
            .index_replacement("s1", &[assistant_msg("summary of the plan")])
            .await
            .unwrap();

        // Old content still searchable, flagged compacted.
        let old = index
            .search(&SearchQuery::new("original secret"))
            .await
            .unwrap();
        assert_eq!(old.len(), 1);
        assert!(old[0].compacted);

        // New content is active.
        let new = index
            .search(&SearchQuery::new("summary plan"))
            .await
            .unwrap();
        assert_eq!(new.len(), 1);
        assert!(!new[0].compacted);
    }

    #[tokio::test]
    async fn remove_session_purges_fts() {
        let index = temp_index().await;
        index
            .index_appended("s1", &user_msg("ephemeral data"))
            .await
            .unwrap();
        index.remove_session("s1").await.unwrap();
        let hits = index.search(&SearchQuery::new("ephemeral")).await.unwrap();
        assert!(hits.is_empty());
        assert_eq!(index.indexed_messages().await.unwrap(), 0);
    }

    // ── discovery ───────────────────────────────────────────────

    #[tokio::test]
    async fn discover_groups_by_session_with_context_and_bookends() {
        let index = temp_index().await;
        for i in 0..10 {
            index
                .index_appended("s1", &user_msg(&format!("filler message {i}")))
                .await
                .unwrap();
        }
        index
            .index_appended("s1", &user_msg("the needle is here"))
            .await
            .unwrap();
        index
            .index_appended("s1", &assistant_msg("resolution after the needle"))
            .await
            .unwrap();

        let matches = index
            .discover(&SearchQuery::new("needle"), &[], 2)
            .await
            .unwrap();
        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert_eq!(m.session_key, "s1");
        assert_eq!(m.hit.seq, 10);
        assert!(m.context.iter().any(|c| c.is_match && c.seq == 10));
        assert_eq!(m.bookend_start.len(), 3);
        assert_eq!(m.bookend_start[0].seq, 0);
        assert!(m.messages_before > 0);
        // Window [8..12] truncated at 11 → nothing after.
        assert_eq!(m.messages_after, 0);
        // Bookend overlap with start is dropped, no duplicate seqs.
        let end_seqs: Vec<i64> = m.bookend_end.iter().map(|c| c.seq).collect();
        assert!(end_seqs.iter().all(|seq| *seq >= 9));
    }

    #[tokio::test]
    async fn discover_demotes_prefixed_sessions() {
        let index = temp_index().await;
        index
            .index_appended("cron:job1", &user_msg("deploy checklist steps"))
            .await
            .unwrap();
        index
            .index_appended("main", &user_msg("deploy checklist"))
            .await
            .unwrap();

        let matches = index
            .discover(&SearchQuery::new("deploy checklist"), &["cron:".into()], 2)
            .await
            .unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].session_key, "main");
        assert_eq!(matches[1].session_key, "cron:job1");
    }

    // ── backfill ────────────────────────────────────────────────

    #[tokio::test]
    async fn sync_session_is_incremental_and_idempotent() {
        let index = temp_index().await;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::SessionStore::new(dir.path().to_path_buf());

        store.append("s1", &user_msg("first line")).await.unwrap();
        store
            .append("s1", &assistant_msg("second line"))
            .await
            .unwrap();

        assert_eq!(index.sync_session(&store, "s1").await.unwrap(), 2);
        assert_eq!(index.sync_session(&store, "s1").await.unwrap(), 0);

        store.append("s1", &user_msg("third line")).await.unwrap();
        assert_eq!(index.sync_session(&store, "s1").await.unwrap(), 1);

        let hits = index.search(&SearchQuery::new("third line")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 2);
    }

    #[tokio::test]
    async fn sync_all_walks_every_session() {
        let index = temp_index().await;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::SessionStore::new(dir.path().to_path_buf());

        store.append("a", &user_msg("alpha content")).await.unwrap();
        store.append("b", &user_msg("beta content")).await.unwrap();

        assert_eq!(index.sync_all(&store).await.unwrap(), 2);
        assert_eq!(index.indexed_messages().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn seq_preserved_across_backfill_and_live_appends() {
        let index = temp_index().await;
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::SessionStore::new(dir.path().to_path_buf());

        store.append("s1", &user_msg("backfilled")).await.unwrap();
        index.sync_all(&store).await.unwrap();

        let live = user_msg("live append");
        store.append("s1", &live).await.unwrap();
        index.index_appended("s1", &live).await.unwrap();

        // Live append continues seq after backfilled rows; re-sync adds nothing.
        assert_eq!(index.sync_session(&store, "s1").await.unwrap(), 0);
        let hits = index
            .search(&SearchQuery::new("live append"))
            .await
            .unwrap();
        assert_eq!(hits[0].seq, 1);
    }
}
