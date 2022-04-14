-- Add migration script here
CREATE TABLE pastes (
    key VARCHAR(8) PRIMARY KEY,
    contents TEXT,
    expires TIMESTAMPTZ
)