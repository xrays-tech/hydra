-- Append-only admin mutation audit (H6.6).
--
-- One row per authenticated admin mutation ATTEMPT, written best-effort by the
-- admin router after the handler returns. This is deliberately NOT part of the
-- config-write transaction: an audit-schema problem must never fail a mutation,
-- and the mutation remains the source of truth. No secrets or request bodies are
-- stored. Retention/export/tamper-evidence are a separate decision.

CREATE TABLE admin_audit (
    id          BIGSERIAL PRIMARY KEY,
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    method      TEXT NOT NULL,
    path        TEXT NOT NULL,
    entity_type TEXT,
    request_id  TEXT,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
