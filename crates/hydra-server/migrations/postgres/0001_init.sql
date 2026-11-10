-- Hydra greenfield schema — PostgreSQL baseline (Phase 2a).
--
-- Replaces the historical SQLite 0001..0015 replay with one portable baseline.
-- Conventions from `.slim/deepwork/shared-db-no-raft.md` §6.2:
--   * VARCHAR(255) only for ids / keys / hashes / names / external_id / domain
--     and wildcard role dimensions;
--   * TEXT for endpoints, PEM, billing_reason and limit-role matching_* values;
--   * BIGINT for every integer/flag column (matches existing i64 row decoding);
--   * BYTEA for sealed secrets;
--   * expires_at stays VARCHAR(64) holding an RFC3339-Z string because existing
--     code compares it lexicographically;
--   * `hydra_user` replaces `user` (reserved word in PostgreSQL).
--
-- Timestamps are DB-native; `db::init_pool` pins the session timezone to UTC in
-- Phase 2b and row structs decode to chrono::NaiveDateTime.

CREATE TABLE hydra_lock (
    id BIGINT PRIMARY KEY
);

CREATE TABLE config_revision (
    id      BIGINT PRIMARY KEY,
    version BIGINT NOT NULL
);

CREATE TABLE provider (
    id                     VARCHAR(255) PRIMARY KEY,
    provider_code          VARCHAR(255) NOT NULL UNIQUE,
    name                   VARCHAR(255) NOT NULL,
    endpoint               TEXT NOT NULL,
    weight                 BIGINT NOT NULL DEFAULT 1 CHECK (weight >= 0),
    created_at             TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at             TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    max_concurrency        BIGINT,
    max_queue_depth        BIGINT,
    queue_wait_timeout_ms  BIGINT
);

CREATE TABLE provider_model (
    id          VARCHAR(255) PRIMARY KEY,
    model_key   VARCHAR(255) NOT NULL,
    name        VARCHAR(255) NOT NULL,
    provider_id VARCHAR(255) NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    status      BIGINT NOT NULL DEFAULT 1 CHECK (status IN (1, 0, -1)),
    UNIQUE (model_key, provider_id)
);
CREATE INDEX idx_provider_model_key ON provider_model(model_key);
CREATE INDEX idx_provider_model_provider ON provider_model(provider_id);

CREATE TABLE provider_key (
    id                 VARCHAR(255) PRIMARY KEY,
    provider_id        VARCHAR(255) NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    api_key_ciphertext BYTEA,
    api_key_nonce      BYTEA,
    key_version        BIGINT NOT NULL DEFAULT 1,
    created_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_provider_key_provider ON provider_key(provider_id);

CREATE TABLE tenant (
    id                 VARCHAR(255) PRIMARY KEY,
    name               VARCHAR(255) NOT NULL,
    domain             VARCHAR(255) NOT NULL UNIQUE,
    cert_key           TEXT,
    cert_file          TEXT,
    cert_pem           TEXT,
    cert_key_ciphertext BYTEA,
    cert_key_nonce     BYTEA,
    cert_key_version   BIGINT,
    enabled            BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE tenant_provider (
    id          VARCHAR(255) PRIMARY KEY,
    tenant_id   VARCHAR(255) NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    provider_id VARCHAR(255) NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    UNIQUE (tenant_id, provider_id)
);
CREATE INDEX idx_tenant_provider_provider ON tenant_provider(provider_id);

CREATE TABLE tenant_model (
    id        VARCHAR(255) PRIMARY KEY,
    tenant_id VARCHAR(255) NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    model_key VARCHAR(255) NOT NULL,
    UNIQUE (tenant_id, model_key)
);

CREATE TABLE limit_role (
    id                VARCHAR(255) PRIMARY KEY,
    name              VARCHAR(255) NOT NULL,
    matching_key      TEXT,
    matching_model    TEXT,
    matching_tenant   TEXT,
    matching_provider TEXT,
    limit_count       BIGINT,
    limit_token       BIGINT,
    limit_window      VARCHAR(8) NOT NULL CHECK (limit_window IN ('m', 'h', 'd')),
    enabled           BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at        TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE user_group (
    id         VARCHAR(255) PRIMARY KEY,
    tenant_id  VARCHAR(255) NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    name       VARCHAR(255) NOT NULL,
    enabled    BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, name)
);

CREATE TABLE hydra_user (
    id                 VARCHAR(255) PRIMARY KEY,
    tenant_id          VARCHAR(255) NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    user_group_id      VARCHAR(255) NOT NULL REFERENCES user_group(id) ON DELETE RESTRICT,
    external_id        VARCHAR(255),
    name               VARCHAR(255) NOT NULL,
    enabled            BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    billing_status     VARCHAR(32) NOT NULL DEFAULT 'active'
                       CHECK (billing_status IN ('active', 'delinquent')),
    billing_reason     TEXT,
    billing_updated_at TIMESTAMP,
    created_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at         TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, external_id)
);
CREATE INDEX idx_hydra_user_tenant_group ON hydra_user(tenant_id, user_group_id);
CREATE INDEX idx_hydra_user_group ON hydra_user(user_group_id);

CREATE TABLE user_api_key (
    id         VARCHAR(255) PRIMARY KEY,
    user_id    VARCHAR(255) NOT NULL REFERENCES hydra_user(id) ON DELETE CASCADE,
    name       VARCHAR(255) NOT NULL,
    key_hash   VARCHAR(255) NOT NULL UNIQUE,
    key_prefix VARCHAR(255) NOT NULL,
    key_last4  VARCHAR(255) NOT NULL,
    enabled    BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    expires_at VARCHAR(64),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_user_api_key_user ON user_api_key(user_id);

CREATE TABLE tenant_access_token (
    id           VARCHAR(255) PRIMARY KEY,
    tenant_id    VARCHAR(255) NOT NULL REFERENCES tenant(id) ON DELETE CASCADE,
    name         VARCHAR(255) NOT NULL,
    token_hash   VARCHAR(255) NOT NULL UNIQUE,
    token_prefix VARCHAR(255) NOT NULL,
    token_last4  VARCHAR(255) NOT NULL,
    enabled      BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    expires_at   VARCHAR(64),
    created_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at   TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, name)
);

CREATE TABLE global_role (
    id            VARCHAR(255) PRIMARY KEY,
    name          VARCHAR(255) NOT NULL,
    tenant_id     VARCHAR(255) NOT NULL DEFAULT '*',
    user_group_id VARCHAR(255) NOT NULL DEFAULT '*',
    user_id       VARCHAR(255) NOT NULL DEFAULT '*',
    api_key_id    VARCHAR(255) NOT NULL DEFAULT '*',
    model_key     VARCHAR(255) NOT NULL DEFAULT '*',
    enabled       BIGINT NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_global_role_tenant ON global_role(tenant_id);

CREATE TABLE global_role_provider (
    id          VARCHAR(255) PRIMARY KEY,
    role_id     VARCHAR(255) NOT NULL REFERENCES global_role(id) ON DELETE CASCADE,
    provider_id VARCHAR(255) NOT NULL REFERENCES provider(id) ON DELETE CASCADE,
    UNIQUE (role_id, provider_id)
);
CREATE INDEX idx_global_role_provider_provider ON global_role_provider(provider_id);

-- Seed the single write-serialization row and revision row. The write path
-- re-seeds hydra_lock idempotently before its first `SELECT ... FOR UPDATE`, so
-- two first writers cannot deadlock on insert.
INSERT INTO hydra_lock (id) VALUES (1) ON CONFLICT (id) DO NOTHING;
INSERT INTO config_revision (id, version) VALUES (1, 0) ON CONFLICT (id) DO NOTHING;
