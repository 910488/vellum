CREATE TABLE IF NOT EXISTS schema_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS upstream_epochs (
    epoch INTEGER PRIMARY KEY,
    started_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    end_reason TEXT,
    codex_version TEXT,
    codex_home TEXT,
    socket_path TEXT,
    server_identity_json BLOB,
    compatibility_state TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS remote_threads (
    thread_id TEXT PRIMARY KEY,
    cwd TEXT,
    title TEXT,
    source_kind TEXT,
    latest_status TEXT NOT NULL,
    loaded INTEGER NOT NULL DEFAULT 0,
    active_turn_id TEXT,
    last_upstream_epoch INTEGER,
    last_event_seq INTEGER NOT NULL DEFAULT 0,
    last_snapshot_seq INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    metadata_json BLOB,
    FOREIGN KEY(last_upstream_epoch) REFERENCES upstream_epochs(epoch)
);

CREATE TABLE IF NOT EXISTS remote_turns (
    turn_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    upstream_epoch INTEGER NOT NULL,
    state TEXT NOT NULL,
    started_at_ms INTEGER,
    completed_at_ms INTEGER,
    usage_json BLOB,
    final_error_json BLOB,
    FOREIGN KEY(thread_id) REFERENCES remote_threads(thread_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_remote_turns_thread
ON remote_turns(thread_id, started_at_ms);

CREATE TABLE IF NOT EXISTS remote_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL,
    turn_id TEXT,
    item_id TEXT,
    upstream_epoch INTEGER NOT NULL,
    method TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    received_at_ms INTEGER NOT NULL,
    payload_ciphertext BLOB NOT NULL,
    payload_sha256 TEXT NOT NULL,
    flags INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(thread_id) REFERENCES remote_threads(thread_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_remote_events_thread_seq
ON remote_events(thread_id, seq);
CREATE INDEX IF NOT EXISTS idx_remote_events_turn_seq
ON remote_events(turn_id, seq);

CREATE TABLE IF NOT EXISTS pending_server_requests (
    local_token TEXT PRIMARY KEY,
    upstream_epoch INTEGER NOT NULL,
    upstream_request_id TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    item_id TEXT,
    method TEXT NOT NULL,
    state TEXT NOT NULL,
    request_ciphertext BLOB NOT NULL,
    response_ciphertext BLOB,
    created_at_ms INTEGER NOT NULL,
    presented_at_ms INTEGER,
    resolved_at_ms INTEGER,
    UNIQUE(upstream_epoch, upstream_request_id)
);
CREATE INDEX IF NOT EXISTS idx_pending_requests_thread
ON pending_server_requests(thread_id, state);

CREATE TABLE IF NOT EXISTS client_devices (
    device_id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    public_key TEXT,
    token_hash TEXT,
    capabilities_json BLOB,
    created_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER,
    revoked_at_ms INTEGER
);

CREATE TABLE IF NOT EXISTS client_thread_acks (
    device_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    through_seq INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY(device_id, thread_id)
);

CREATE TABLE IF NOT EXISTS client_commands (
    idempotency_key TEXT PRIMARY KEY,
    device_id TEXT NOT NULL,
    thread_id TEXT,
    command_kind TEXT NOT NULL,
    command_ciphertext BLOB NOT NULL,
    state TEXT NOT NULL,
    result_ciphertext BLOB,
    error_ciphertext BLOB,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_client_commands_thread
ON client_commands(thread_id, created_at_ms);

CREATE TABLE IF NOT EXISTS audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at_ms INTEGER NOT NULL,
    actor_type TEXT NOT NULL,
    actor_id TEXT,
    event_kind TEXT NOT NULL,
    thread_id TEXT,
    details_json BLOB
);
