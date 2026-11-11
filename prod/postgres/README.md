# Postgres data directory (bind-mounted at /var/lib/postgresql/data)
#
# Permissions: the `postgres:16` image runs as uid/gid 999. The host directory must be owned by
# that uid or Postgres refuses to start ("could not change directory" / "data directory has wrong
# ownership"). ops/setup-dirs.sh does this:
#
#   chown -R 999:999 prod/data/postgres
#   chmod 700 prod/data/postgres
#
# If you created the directory by hand as root and an earlier Postgres version touched it, wipe it
# BEFORE first start (an initialized-but-wrong-owner PGDATA is not repaired by chown):
#
#   rm -rf prod/data/postgres/*        # DESTRUCTIVE: only before the first successful boot
#
# Do not use an NFS/exotic filesystem for PGDATA unless you know its semantics; a local disk (or a
# block-backed volume) is assumed.
