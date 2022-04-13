-- Add migration script here
CREATE TABLE (
    key TEXT PRIMARY KEY,
    data TEXT NOT NULL,
    expiry TIMESTAMPTZ
)
