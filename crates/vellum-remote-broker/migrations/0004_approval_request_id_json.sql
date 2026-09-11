-- Preserve original JSON-RPC request id type (number vs string).
ALTER TABLE pending_server_requests
    ADD COLUMN upstream_request_id_json BLOB;

UPDATE pending_server_requests
SET upstream_request_id_json = CAST(upstream_request_id AS BLOB)
WHERE upstream_request_id_json IS NULL;
