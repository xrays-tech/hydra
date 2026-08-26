-- Embed tenant TLS certificates as PEM content (cluster P0a: remove the
-- shared cert-volume dependency). The public cert PEM is plaintext (public
-- material); the private key is AES-256-GCM sealed at rest with the same
-- master key as provider api-keys (see 0003_provider_key_encryption.sql).
--
-- The legacy `cert_file` / `cert_key` PATH columns are kept for read
-- compatibility (single-node pre-0007 rows); the loader resolves content
-- first and falls back to paths. New writes store content (migration 0007 is
-- additive; no hard cutover, unlike 0003 — path rows are backfilled by the
-- leader at startup).

ALTER TABLE tenant ADD COLUMN cert_pem TEXT;                 -- public cert PEM
ALTER TABLE tenant ADD COLUMN cert_key_ciphertext BLOB;      -- AES-256-GCM
ALTER TABLE tenant ADD COLUMN cert_key_nonce BLOB;
ALTER TABLE tenant ADD COLUMN cert_key_version INTEGER;
