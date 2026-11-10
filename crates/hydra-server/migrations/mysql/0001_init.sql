-- Hydra greenfield schema — MySQL baseline (Phase 2a).
--
-- Replaces the historical SQLite 0001..0015 replay with one portable baseline.
-- Conventions from `.slim/deepwork/shared-db-no-raft.md` §6.2:
--   * VARCHAR(255) only for ids / keys / hashes / names / external_id / domain
--     and wildcard role dimensions;
--   * TEXT for endpoints, PEM, billing_reason and limit-role matching_* values;
--   * BIGINT for every integer/flag column (matches existing i64 row decoding);
--   * BLOB for sealed secrets;
--   * expires_at stays VARCHAR(64) holding an RFC3339-Z string because existing
--     code compares it lexicographically;
--   * `hydra_user` replaces `user`.
--
-- Every table is InnoDB + utf8mb4. Composite unique keys stay under the
-- 3072-byte limit (max here is 2 × VARCHAR(255) = 2040 bytes). Foreign-key
-- columns have explicit indexes as MySQL requires.
--
-- Timestamps are DB-native DATETIME; `db::init_pool` pins the session timezone
-- to '+00:00' in Phase 2b and row structs decode to chrono::NaiveDateTime.

CREATE TABLE hydra_lock (
    id BIGINT PRIMARY KEY
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE config_revision (
    id      BIGINT PRIMARY KEY,
    version BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE provider (
    id                    VARCHAR(255) PRIMARY KEY,
    provider_code         VARCHAR(255) NOT NULL UNIQUE,
    name                  VARCHAR(255) NOT NULL,
    endpoint              TEXT NOT NULL,
    weight                BIGINT NOT NULL DEFAULT 1,
    created_at            DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at            DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    max_concurrency       BIGINT,
    max_queue_depth       BIGINT,
    queue_wait_timeout_ms BIGINT,
    CONSTRAINT chk_provider_weight CHECK (weight >= 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE provider_model (
    id          VARCHAR(255) PRIMARY KEY,
    model_key   VARCHAR(255) NOT NULL,
    name        VARCHAR(255) NOT NULL,
    provider_id VARCHAR(255) NOT NULL,
    status      BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT chk_provider_model_status CHECK (status IN (1, 0, -1)),
    CONSTRAINT fk_provider_model_provider FOREIGN KEY (provider_id)
        REFERENCES provider(id) ON DELETE CASCADE,
    UNIQUE KEY uq_provider_model_key_provider (model_key, provider_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_provider_model_key ON provider_model(model_key);
CREATE INDEX idx_provider_model_provider ON provider_model(provider_id);

CREATE TABLE provider_key (
    id                 VARCHAR(255) PRIMARY KEY,
    provider_id        VARCHAR(255) NOT NULL,
    api_key_ciphertext BLOB,
    api_key_nonce      BLOB,
    key_version        BIGINT NOT NULL DEFAULT 1,
    created_at         DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_provider_key_provider FOREIGN KEY (provider_id)
        REFERENCES provider(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_provider_key_provider ON provider_key(provider_id);

CREATE TABLE tenant (
    id                  VARCHAR(255) PRIMARY KEY,
    name                VARCHAR(255) NOT NULL,
    domain              VARCHAR(255) NOT NULL UNIQUE,
    cert_key            TEXT,
    cert_file           TEXT,
    cert_pem            TEXT,
    cert_key_ciphertext BLOB,
    cert_key_nonce      BLOB,
    cert_key_version    BIGINT,
    enabled             BIGINT NOT NULL DEFAULT 1,
    created_at          DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at          DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT chk_tenant_enabled CHECK (enabled IN (0, 1))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE tenant_provider (
    id          VARCHAR(255) PRIMARY KEY,
    tenant_id   VARCHAR(255) NOT NULL,
    provider_id VARCHAR(255) NOT NULL,
    CONSTRAINT fk_tenant_provider_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenant(id) ON DELETE CASCADE,
    CONSTRAINT fk_tenant_provider_provider FOREIGN KEY (provider_id)
        REFERENCES provider(id) ON DELETE CASCADE,
    UNIQUE KEY uq_tenant_provider_tenant_provider (tenant_id, provider_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_tenant_provider_provider ON tenant_provider(provider_id);

CREATE TABLE tenant_model (
    id        VARCHAR(255) PRIMARY KEY,
    tenant_id VARCHAR(255) NOT NULL,
    model_key VARCHAR(255) NOT NULL,
    CONSTRAINT fk_tenant_model_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenant(id) ON DELETE CASCADE,
    UNIQUE KEY uq_tenant_model_tenant_model (tenant_id, model_key)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE limit_role (
    id                VARCHAR(255) PRIMARY KEY,
    name              VARCHAR(255) NOT NULL,
    matching_key      TEXT,
    matching_model    TEXT,
    matching_tenant   TEXT,
    matching_provider TEXT,
    limit_count       BIGINT,
    limit_token       BIGINT,
    limit_window      VARCHAR(8) NOT NULL,
    enabled           BIGINT NOT NULL DEFAULT 1,
    created_at        DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT chk_limit_role_window CHECK (limit_window IN ('m', 'h', 'd')),
    CONSTRAINT chk_limit_role_enabled CHECK (enabled IN (0, 1))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE user_group (
    id         VARCHAR(255) PRIMARY KEY,
    tenant_id  VARCHAR(255) NOT NULL,
    name       VARCHAR(255) NOT NULL,
    enabled    BIGINT NOT NULL DEFAULT 1,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_user_group_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenant(id) ON DELETE CASCADE,
    CONSTRAINT chk_user_group_enabled CHECK (enabled IN (0, 1)),
    UNIQUE KEY uq_user_group_tenant_name (tenant_id, name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE hydra_user (
    id                 VARCHAR(255) PRIMARY KEY,
    tenant_id          VARCHAR(255) NOT NULL,
    user_group_id      VARCHAR(255) NOT NULL,
    external_id        VARCHAR(255),
    name               VARCHAR(255) NOT NULL,
    enabled            BIGINT NOT NULL DEFAULT 1,
    billing_status     VARCHAR(32) NOT NULL DEFAULT 'active',
    billing_reason     TEXT,
    billing_updated_at DATETIME,
    created_at         DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at         DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_hydra_user_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenant(id) ON DELETE CASCADE,
    CONSTRAINT fk_hydra_user_group FOREIGN KEY (user_group_id)
        REFERENCES user_group(id) ON DELETE RESTRICT,
    CONSTRAINT chk_hydra_user_enabled CHECK (enabled IN (0, 1)),
    CONSTRAINT chk_hydra_user_billing_status
        CHECK (billing_status IN ('active', 'delinquent')),
    UNIQUE KEY uq_hydra_user_tenant_external (tenant_id, external_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_hydra_user_tenant_group ON hydra_user(tenant_id, user_group_id);
CREATE INDEX idx_hydra_user_group ON hydra_user(user_group_id);

CREATE TABLE user_api_key (
    id         VARCHAR(255) PRIMARY KEY,
    user_id    VARCHAR(255) NOT NULL,
    name       VARCHAR(255) NOT NULL,
    key_hash   VARCHAR(255) NOT NULL UNIQUE,
    key_prefix VARCHAR(255) NOT NULL,
    key_last4  VARCHAR(255) NOT NULL,
    enabled    BIGINT NOT NULL DEFAULT 1,
    expires_at VARCHAR(64),
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_user_api_key_user FOREIGN KEY (user_id)
        REFERENCES hydra_user(id) ON DELETE CASCADE,
    CONSTRAINT chk_user_api_key_enabled CHECK (enabled IN (0, 1))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_user_api_key_user ON user_api_key(user_id);

CREATE TABLE tenant_access_token (
    id           VARCHAR(255) PRIMARY KEY,
    tenant_id    VARCHAR(255) NOT NULL,
    name         VARCHAR(255) NOT NULL,
    token_hash   VARCHAR(255) NOT NULL UNIQUE,
    token_prefix VARCHAR(255) NOT NULL,
    token_last4  VARCHAR(255) NOT NULL,
    enabled      BIGINT NOT NULL DEFAULT 1,
    expires_at   VARCHAR(64),
    created_at   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_tenant_access_token_tenant FOREIGN KEY (tenant_id)
        REFERENCES tenant(id) ON DELETE CASCADE,
    CONSTRAINT chk_tenant_access_token_enabled CHECK (enabled IN (0, 1)),
    UNIQUE KEY uq_tenant_access_token_tenant_name (tenant_id, name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE global_role (
    id            VARCHAR(255) PRIMARY KEY,
    name          VARCHAR(255) NOT NULL,
    tenant_id     VARCHAR(255) NOT NULL DEFAULT '*',
    user_group_id VARCHAR(255) NOT NULL DEFAULT '*',
    user_id       VARCHAR(255) NOT NULL DEFAULT '*',
    api_key_id    VARCHAR(255) NOT NULL DEFAULT '*',
    model_key     VARCHAR(255) NOT NULL DEFAULT '*',
    enabled       BIGINT NOT NULL DEFAULT 1,
    created_at    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT chk_global_role_enabled CHECK (enabled IN (0, 1))
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_global_role_tenant ON global_role(tenant_id);

CREATE TABLE global_role_provider (
    id          VARCHAR(255) PRIMARY KEY,
    role_id     VARCHAR(255) NOT NULL,
    provider_id VARCHAR(255) NOT NULL,
    CONSTRAINT fk_global_role_provider_role FOREIGN KEY (role_id)
        REFERENCES global_role(id) ON DELETE CASCADE,
    CONSTRAINT fk_global_role_provider_provider FOREIGN KEY (provider_id)
        REFERENCES provider(id) ON DELETE CASCADE,
    UNIQUE KEY uq_global_role_provider_role_provider (role_id, provider_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
CREATE INDEX idx_global_role_provider_provider ON global_role_provider(provider_id);

-- Seed the single write-serialization row and revision row. The write path
-- re-seeds hydra_lock idempotently before its first `SELECT ... FOR UPDATE`, so
-- two first writers cannot deadlock on insert.
INSERT INTO hydra_lock (id) VALUES (1) ON DUPLICATE KEY UPDATE id = id;
INSERT INTO config_revision (id, version) VALUES (1, 0) ON DUPLICATE KEY UPDATE version = version;
