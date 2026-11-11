-- Append-only admin mutation audit (H6.6).
--
-- One row per authenticated admin mutation ATTEMPT, written best-effort by the
-- admin router after the handler returns. This is deliberately NOT part of the
-- config-write transaction: an audit-schema problem must never fail a mutation,
-- and the mutation remains the source of truth. No secrets or request bodies are
-- stored. Retention/export/tamper-evidence are a separate decision.

CREATE TABLE admin_audit (
    id          BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
    actor       VARCHAR(64) NOT NULL,
    action      VARCHAR(64) NOT NULL,
    method      VARCHAR(16) NOT NULL,
    path        TEXT NOT NULL,
    entity_type VARCHAR(64),
    request_id  VARCHAR(128),
    created_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;
