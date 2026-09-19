CREATE TABLE IF NOT EXISTS nulang_capacity_provider_ledgers (
    provider_id TEXT PRIMARY KEY,
    generation BIGINT NOT NULL CHECK (generation >= 1),
    snapshot JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS nulang_capacity_provider_ledgers_updated_at_idx
    ON nulang_capacity_provider_ledgers (updated_at);
