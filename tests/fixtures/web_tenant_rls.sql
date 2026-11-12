-- Disposable test schema only. Do not apply as a production migration.
-- Replace __SCHEMA__ with a generated ASCII identifier, never user input.
CREATE SCHEMA __SCHEMA__;
CREATE TABLE __SCHEMA__.records (
    id bigint PRIMARY KEY,
    tenant_id text NOT NULL,
    payload text NOT NULL
);
ALTER TABLE __SCHEMA__.records ENABLE ROW LEVEL SECURITY;
-- FORCE matters: the ordinary table owner must not bypass its own policy.
ALTER TABLE __SCHEMA__.records FORCE ROW LEVEL SECURITY;
CREATE POLICY scoped_records ON __SCHEMA__.records FOR ALL
    USING (tenant_id = NULLIF(current_setting('nulang.tenant_id', true), ''))
    WITH CHECK (tenant_id = NULLIF(current_setting('nulang.tenant_id', true), ''));
