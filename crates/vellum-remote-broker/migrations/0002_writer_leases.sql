CREATE TABLE IF NOT EXISTS writer_leases (
    thread_id TEXT PRIMARY KEY,
    lease_id TEXT NOT NULL UNIQUE,
    device_id TEXT NOT NULL,
    acquired_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    FOREIGN KEY(device_id) REFERENCES client_devices(device_id)
);
