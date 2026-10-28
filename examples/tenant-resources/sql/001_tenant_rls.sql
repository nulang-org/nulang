-- Disposable reference schema, NOT a migration for an existing production DB.
-- Provision a distinct PostgreSQL login per tenant with out-of-band verified
-- authentication. session_user is the authenticated *database* login, not a
-- client-provided tenant claim or user-controlled custom GUC.
CREATE SCHEMA tenant_example;
CREATE SCHEMA tenant_example_private;
REVOKE ALL ON SCHEMA tenant_example_private FROM PUBLIC;

CREATE TABLE tenant_example_private.login_tenants (
    login_name name PRIMARY KEY,
    tenant_id text NOT NULL CHECK (tenant_id <> '')
);
REVOKE ALL ON tenant_example_private.login_tenants FROM PUBLIC;

-- SECURITY DEFINER limits the role/tenant lookup to a protected mapping.
-- Fixed search_path prevents function-name/operator hijacking by untrusted
-- schemas. session_user does not change across SET ROLE.
CREATE FUNCTION tenant_example_private.current_tenant()
RETURNS text
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path = pg_catalog
AS $$
    SELECT mapping.tenant_id
    FROM tenant_example_private.login_tenants AS mapping
    WHERE mapping.login_name = session_user::name
$$;
REVOKE ALL ON FUNCTION tenant_example_private.current_tenant() FROM PUBLIC;

CREATE TABLE tenant_example.billboard_faces (
    tenant_id text NOT NULL CHECK (tenant_id <> ''),
    id text NOT NULL,
    name text NOT NULL,
    active boolean NOT NULL DEFAULT true,
    PRIMARY KEY (tenant_id, id)
);
CREATE TABLE tenant_example.campaigns (
    tenant_id text NOT NULL CHECK (tenant_id <> ''),
    id text NOT NULL,
    face_id text NOT NULL,
    title text NOT NULL,
    PRIMARY KEY (tenant_id, id),
    CONSTRAINT campaigns_face_tenant_fk
        FOREIGN KEY (tenant_id, face_id)
        REFERENCES tenant_example.billboard_faces (tenant_id, id)
);

ALTER TABLE tenant_example.billboard_faces ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_example.billboard_faces FORCE ROW LEVEL SECURITY;
ALTER TABLE tenant_example.campaigns ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_example.campaigns FORCE ROW LEVEL SECURITY;

CREATE POLICY face_tenant_boundary ON tenant_example.billboard_faces
    FOR ALL USING (tenant_id = tenant_example_private.current_tenant())
    WITH CHECK (tenant_id = tenant_example_private.current_tenant());
CREATE POLICY campaign_tenant_boundary ON tenant_example.campaigns
    FOR ALL USING (tenant_id = tenant_example_private.current_tenant())
    WITH CHECK (tenant_id = tenant_example_private.current_tenant());
