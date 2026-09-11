-- Per-thread replay cursor separate from global audit ordering (seq).
ALTER TABLE remote_events ADD COLUMN thread_seq INTEGER;

-- Backfill dense per-thread sequences from existing global order.
UPDATE remote_events
SET thread_seq = (
    SELECT COUNT(*)
    FROM remote_events AS earlier
    WHERE earlier.thread_id = remote_events.thread_id
      AND earlier.seq <= remote_events.seq
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_remote_events_thread_thread_seq
ON remote_events(thread_id, thread_seq);

-- last_event_seq is now a per-thread cursor, not the global audit seq.
UPDATE remote_threads
SET last_event_seq = COALESCE((
    SELECT MAX(e.thread_seq)
    FROM remote_events e
    WHERE e.thread_id = remote_threads.thread_id
), 0);

-- Old snapshots/acks were written with global seq and are unsafe to reuse.
DELETE FROM remote_snapshots;
UPDATE remote_threads SET last_snapshot_seq = NULL;
DELETE FROM client_thread_acks;
