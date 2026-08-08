-- Searchable projection of session transcripts.
-- Owned by: moltis-sessions crate
--
-- JSONL files under <data_dir>/sessions/v1/ remain the transcript source of
-- truth; these tables are a derived index used for cross-session recall.
-- `active`/`compacted` mirror transcript state after compaction: rows that
-- were summarized away keep `compacted = 1` so their content stays searchable
-- even though it is no longer in the live context window.

CREATE TABLE IF NOT EXISTS session_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_key TEXT    NOT NULL,
    seq         INTEGER NOT NULL,
    role        TEXT    NOT NULL,
    content     TEXT    NOT NULL,
    tool_name   TEXT,
    created_at  INTEGER NOT NULL,
    active      INTEGER NOT NULL DEFAULT 1,
    compacted   INTEGER NOT NULL DEFAULT 0,
    UNIQUE (session_key, seq)
);

CREATE INDEX IF NOT EXISTS idx_session_messages_key_seq
    ON session_messages(session_key, seq);

-- External-content FTS5 index over message text. Content rows live in
-- session_messages; triggers keep the index in sync. Flag-only updates
-- (active/compacted) deliberately do not fire the content triggers, so
-- compaction does not rewrite the FTS index.
CREATE VIRTUAL TABLE IF NOT EXISTS session_messages_fts USING fts5(
    content,
    tool_name,
    content='session_messages',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS session_messages_fts_ai
AFTER INSERT ON session_messages BEGIN
    INSERT INTO session_messages_fts(rowid, content, tool_name)
    VALUES (new.id, new.content, new.tool_name);
END;

CREATE TRIGGER IF NOT EXISTS session_messages_fts_ad
AFTER DELETE ON session_messages BEGIN
    INSERT INTO session_messages_fts(session_messages_fts, rowid, content, tool_name)
    VALUES ('delete', old.id, old.content, old.tool_name);
END;

CREATE TRIGGER IF NOT EXISTS session_messages_fts_au
AFTER UPDATE OF content, tool_name ON session_messages BEGIN
    INSERT INTO session_messages_fts(session_messages_fts, rowid, content, tool_name)
    VALUES ('delete', old.id, old.content, old.tool_name);
    INSERT INTO session_messages_fts(rowid, content, tool_name)
    VALUES (new.id, new.content, new.tool_name);
END;
