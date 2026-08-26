-- Config version metadata (cluster P1/P2): a single-row key-value store for
-- control-plane bookkeeping. Today only the standby replica's last-applied
-- config version (`config_version`) lives here, so a promoted standby
-- continues the version sequence without resetting it.
CREATE TABLE config_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
