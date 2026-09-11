CREATE TABLE IF NOT EXISTS remote_snapshots (
    thread_id TEXT NOT NULL,
    snapshot_seq INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    payload_ciphertext BLOB NOT NULL,
    payload_sha256 TEXT NOT NULL,
    PRIMARY KEY(thread_id, snapshot_seq),
    FOREIGN KEY(thread_id) REFERENCES remote_threads(thread_id) ON DELETE CASCADE
);
