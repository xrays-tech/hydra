#!/usr/bin/env python3
"""Per-run isolated PostgreSQL / MySQL for the integration drills.

SQLite is retired (Phase 2 of the shared-DB rewrite), so a drill that starts the
real gateway must point it at a disposable real database. This module is the one
owner of that job for the Python integration suite:

* `HYDRA_TEST_DATABASE_URL` is the base URL (never a drill's own database). It
  must name an existing database and the user must be able to create schemas
  (PostgreSQL) or databases (MySQL). There is no default: a drill that needs a
  real database FAILS with setup instructions rather than silently skipping.
* PostgreSQL isolation is a uniquely-named SCHEMA, reached through the
  `options=-csearch_path%3D<schema>` URI parameter — the same mechanism as the
  Rust harness (`db::testing`), so the unqualified migrations land in the
  isolated schema and `DROP SCHEMA ... CASCADE` is enough to clean up.
* MySQL isolation is a uniquely-named DATABASE; the URL's path component is
  replaced.
* Provisioning/dropping uses `psycopg2` / `pymysql` when importable (the local
  developer environment has them), otherwise the `psql` / `mysql` CLI (the CI
  runners have those), and finally a configured `docker exec` target
  (`HYDRA_TEST_DB_CONTAINER`) for environments where only the container ships a
  client. Every failure carries the exact way to fix it.

The schema/database is dropped on `atexit` as a safety net; a drill should call
`cleanup()` explicitly so the CI database is clean before the next drill.
"""

from __future__ import annotations

import atexit
import os
import re
import secrets
import subprocess
import sys
from urllib.parse import unquote, urlsplit

__all__ = [
    "SetupError",
    "backend_of",
    "base_url",
    "isolated",
    "IsolatedDb",
    "url_with_schema",
    "url_with_db",
]

SETUP_HINT = (
    "set HYDRA_TEST_DATABASE_URL to a real base database, e.g.\n"
    "  PostgreSQL: export HYDRA_TEST_DATABASE_URL="
    "postgres://hydra:hydra@127.0.0.1:55432/hydra\n"
    "  MySQL:      export HYDRA_TEST_DATABASE_URL="
    "mysql://hydra:hydra@127.0.0.1:53306/hydra\n"
    "The user needs CREATE SCHEMA (PostgreSQL) or CREATE DATABASE (MySQL). "
    "SQLite is retired; there is no in-memory fallback."
)


class SetupError(RuntimeError):
    """A missing/unusable test database, reported as CANNOT VERIFY by drills."""


_CREATED: list["IsolatedDb"] = []


@atexit.register
def _cleanup_all() -> None:
    for db in list(_CREATED):
        try:
            db.cleanup()
        except Exception:
            pass


def backend_of(url: str) -> str:
    scheme = urlsplit(url).scheme.lower()
    if scheme in ("postgres", "postgresql"):
        return "postgres"
    if scheme in ("mysql", "mariadb"):
        return "mysql"
    raise SetupError(
        f"HYDRA_TEST_DATABASE_URL={url!r} is not a PostgreSQL or MySQL URL. {SETUP_HINT}"
    )


def base_url() -> str:
    """The configured base URL, or a SetupError naming how to set it."""
    value = os.environ.get("HYDRA_TEST_DATABASE_URL", "").strip()
    if not value:
        raise SetupError(f"HYDRA_TEST_DATABASE_URL is not set. {SETUP_HINT}")
    backend_of(value)  # validates the scheme
    return value


def _safe_label(label: str) -> str:
    label = re.sub(r"[^a-z0-9_]+", "_", (label or "drill").lower()).strip("_")
    return (label or "drill")[:24]


def _unique_name(label: str) -> str:
    # Identifier-safe on both backends (<= 63 chars for PG, <= 64 for MySQL).
    return f"hydra_it_{_safe_label(label)}_{os.getpid()}_{secrets.token_hex(4)}"


def url_with_schema(base: str, schema: str) -> str:
    """Pin a PostgreSQL URL's `search_path` to `schema` (URI `options` form)."""
    sep = "&" if "?" in base else "?"
    return f"{base}{sep}options=-csearch_path%3D{schema}"


def url_with_db(base: str, dbname: str) -> str:
    """Replace only the database path component of a MySQL URL."""
    if "?" in base:
        before_query, query = base.split("?", 1)
        suffix = f"?{query}"
    else:
        before_query, suffix = base, ""
    authority_start = before_query.find("://")
    authority_start = 0 if authority_start < 0 else authority_start + 3
    slash = before_query.find("/", authority_start)
    if slash < 0:
        return f"{before_query}/{dbname}{suffix}"
    return f"{before_query[:slash]}/{dbname}{suffix}"


def _run(cmd: list[str], *, env: dict | None = None) -> subprocess.CompletedProcess:
    try:
        proc = subprocess.run(
            cmd, env=env, capture_output=True, text=True, timeout=30, check=False
        )
    except FileNotFoundError as e:
        raise SetupError(
            f"cannot provision the test database: {cmd[0]!r} is not installed and the "
            f"Python driver is unavailable. Install psql/mysql-client, or set "
            f"HYDRA_TEST_DB_CONTAINER to a container that has the client. ({e})"
        ) from e
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()[:800]
        raise SetupError(
            f"database provisioning command failed ({proc.returncode}): "
            f"{' '.join(cmd[:3])} … :: {detail}. {SETUP_HINT}"
        )
    return proc


def _docker_exec(container: str, argv: list[str]) -> list[str]:
    return ["docker", "exec", container, *argv]


def _pg_sql(url: str, sql: str) -> None:
    try:
        import psycopg2  # type: ignore
    except ImportError:
        psycopg2 = None
    if psycopg2 is not None:
        conn = psycopg2.connect(url)
        try:
            conn.autocommit = True
            with conn.cursor() as cur:
                cur.execute(sql)
        finally:
            conn.close()
        return
    container = os.environ.get("HYDRA_TEST_DB_CONTAINER")
    if container:
        _run(_docker_exec(container, ["psql", url, "-v", "ON_ERROR_STOP=1", "-c", sql]))
    else:
        _run(["psql", url, "-v", "ON_ERROR_STOP=1", "-c", sql])


def _mysql_parts(url: str) -> dict:
    p = urlsplit(url)
    return {
        "host": p.hostname or "127.0.0.1",
        "port": p.port or 3306,
        "user": unquote(p.username or "root"),
        "password": unquote(p.password or ""),
        "database": unquote((p.path or "/").lstrip("/")) or None,
    }


def _mysql_sql(url: str, sql: str) -> None:
    try:
        import pymysql  # type: ignore
    except ImportError:
        pymysql = None
    if pymysql is not None:
        p = _mysql_parts(url)
        conn = pymysql.connect(
            host=p["host"],
            port=p["port"],
            user=p["user"],
            password=p["password"],
            database=p["database"],
            autocommit=True,
            connect_timeout=10,
        )
        try:
            with conn.cursor() as cur:
                cur.execute(sql)
        finally:
            conn.close()
        return
    p = _mysql_parts(url)
    argv = [
        "mysql",
        "--protocol=TCP",
        f"--host={p['host']}",
        f"--port={p['port']}",
        f"--user={p['user']}",
        "--execute",
        sql,
    ]
    env = dict(os.environ)
    if p["password"]:
        env["MYSQL_PWD"] = p["password"]
    container = os.environ.get("HYDRA_TEST_DB_CONTAINER")
    if container:
        if p["password"]:
            argv.insert(1, f"--password={p['password']}")
            env.pop("MYSQL_PWD", None)
        _run(_docker_exec(container, argv), env=env)
    else:
        _run(argv, env=env)


def _sql(url: str, backend: str, statement: str) -> None:
    if backend == "postgres":
        _pg_sql(url, statement)
    else:
        _mysql_sql(url, statement)


class IsolatedDb:
    """A uniquely-named PostgreSQL schema / MySQL database with its URL."""

    def __init__(self, base: str, backend: str, name: str, url: str):
        self.base = base
        self.backend = backend
        self.name = name
        self.url = url
        self._dropped = False

    def cleanup(self) -> None:
        if self._dropped:
            return
        self._dropped = True
        if self.backend == "postgres":
            stmt = f'DROP SCHEMA IF EXISTS "{self.name}" CASCADE'
        else:
            stmt = f"DROP DATABASE IF EXISTS `{self.name}`"
        try:
            _sql(self.base, self.backend, stmt)
        except Exception as e:  # cleanup is best-effort; never mask a drill result
            print(
                f"[shared-db] warning: could not drop {self.backend} {self.name}: {e}",
                file=sys.stderr,
            )
        try:
            _CREATED.remove(self)
        except ValueError:
            pass

    def __enter__(self) -> "IsolatedDb":
        return self

    def __exit__(self, *exc) -> None:
        self.cleanup()

    def __repr__(self) -> str:  # no credentials
        return f"IsolatedDb({self.backend}, {self.name})"


def isolated(label: str = "drill") -> IsolatedDb:
    """Create a fresh isolated schema/database; migrations are run by the node."""
    base = base_url()
    backend = backend_of(base)
    name = _unique_name(label)
    if backend == "postgres":
        _sql(base, backend, f'CREATE SCHEMA "{name}"')
        url = url_with_schema(base, name)
    else:
        _sql(
            base,
            backend,
            f"CREATE DATABASE `{name}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        )
        url = url_with_db(base, name)
    db = IsolatedDb(base, backend, name, url)
    _CREATED.append(db)
    return db


def drop_name(name: str) -> None:
    """Drop a schema/database created by `isolated` (used by the shell run scripts)."""
    base = base_url()
    backend = backend_of(base)
    IsolatedDb(base, backend, name, "").cleanup()


def run_sql(url: str, statement: str) -> None:
    """Execute one statement against `url` (used by drills that corrupt a row directly)."""
    _sql(url, backend_of(url), statement)


def _query(url: str, statement: str) -> list[str]:
    """One-column query, through the driver when present and the CLI otherwise."""
    backend = backend_of(url)
    if backend == "postgres":
        try:
            import psycopg2  # type: ignore
        except ImportError:
            psycopg2 = None
        if psycopg2 is not None:
            conn = psycopg2.connect(url)
            try:
                with conn.cursor() as cur:
                    cur.execute(statement)
                    return [str(r[0]) for r in cur.fetchall()]
            finally:
                conn.close()
        proc = subprocess.run(
            ["psql", url, "-Atc", statement],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if proc.returncode != 0:
            raise SetupError(f"database query failed: {(proc.stderr or '').strip()[:500]}")
        return [line for line in proc.stdout.splitlines() if line.strip()]

    p = _mysql_parts(url)
    try:
        import pymysql  # type: ignore
    except ImportError:
        pymysql = None
    if pymysql is not None:
        conn = pymysql.connect(
            host=p["host"],
            port=p["port"],
            user=p["user"],
            password=p["password"],
            database=p["database"],
            autocommit=True,
            connect_timeout=10,
        )
        try:
            with conn.cursor() as cur:
                cur.execute(statement)
                return [str(r[0]) for r in cur.fetchall()]
        finally:
            conn.close()
    argv = [
        "mysql",
        "--protocol=TCP",
        f"--host={p['host']}",
        f"--port={p['port']}",
        f"--user={p['user']}",
        "--batch",
        "--skip-column-names",
        "--execute",
        statement,
    ]
    env = dict(os.environ)
    if p["password"]:
        env["MYSQL_PWD"] = p["password"]
    proc = _run(argv, env=env)
    return [line for line in proc.stdout.splitlines() if line.strip()]


def _ident(backend: str, name: str) -> str:
    return f'"{name}"' if backend == "postgres" else f"`{name}`"


def list_tables(db: "IsolatedDb") -> list[str]:
    if db.backend == "postgres":
        sql = (
            "SELECT table_name FROM information_schema.tables "
            f"WHERE table_schema = '{db.name}' AND table_type = 'BASE TABLE' "
            "ORDER BY table_name"
        )
    else:
        sql = "SHOW TABLES"
    return _query(db.url, sql)


def copy_isolated_rows(src: "IsolatedDb", dst: "IsolatedDb") -> list[str]:
    """Copy the common tables' rows from `src` into an already-migrated `dst`.

    A logical restore: both isolated schemas/databases live on the same server, so
    `INSERT INTO dst.t SELECT * FROM src.t` is a real cross-schema/database copy. The destination
    must have its tables already (start a node once, or run migrations); `_sqlx_migrations` is
    skipped so the restored database keeps its own migration state.
    """
    dependency_order = [
        "hydra_lock", "config_revision", "provider", "provider_model", "provider_key",
        "tenant", "tenant_provider", "tenant_model", "limit_role", "user_group",
        "hydra_user", "user_api_key", "tenant_access_token", "global_role",
        "global_role_provider",
    ]
    rank = {name: i for i, name in enumerate(dependency_order)}
    tables = sorted(
        (set(list_tables(src)) & set(list_tables(dst))) - {"_sqlx_migrations"},
        key=lambda t: (rank.get(t, len(rank)), t),
    )
    b = src.backend
    for table in tables:
        s = f"{_ident(b, src.name)}.{_ident(b, table)}"
        d = f"{_ident(b, dst.name)}.{_ident(b, table)}"
        if table == "hydra_lock":
            # Every migrated database seeds this row; a copy would collide on id=1.
            continue
        if table == "config_revision":
            # Same singleton row: carry the source revision's value instead of the row.
            run_sql(
                dst.url,
                f"UPDATE {d} SET version = (SELECT version FROM {s})",
            )
            continue
        run_sql(dst.url, f"INSERT INTO {d} SELECT * FROM {s}")
    return tables


_ENV_URLS: dict[str, str] = {}


def env_db_url(label: str = "drill") -> str:
    """A process-cached isolated URL, for hand-built node environments.

    Tests that already have an `env = {... "HYDRA_DB_URL": ...}` dictionary only need to replace
    the SQLite path with `env_db_url(label)`. The schema/database is created on first use and
    dropped by the module's atexit hook when the drill process exits. A missing
    `HYDRA_TEST_DATABASE_URL` exits 2 with setup instructions (CANNOT VERIFY), not a traceback.
    """
    if label not in _ENV_URLS:
        try:
            _ENV_URLS[label] = isolated(label).url
        except SetupError as e:
            print(f"[shared-db] CANNOT VERIFY: {e}", file=sys.stderr)
            raise SystemExit(2) from e
    return _ENV_URLS[label]


def _main(argv: list[str]) -> int:
    """`create <label>` prints `name<TAB>url`; `drop <name>` cleans up.

    Shell scripts cannot import the helper, and an in-process `isolated()` would drop the
    schema at its own exit — so the CLI owns the lifetime across the script.
    """
    try:
        if len(argv) == 2 and argv[0] == "create":
            db = isolated(argv[1])
            _CREATED.remove(db)  # the shell drop owns it from here
            print(f"{db.name}\t{db.url}")
            return 0
        if len(argv) == 2 and argv[0] == "drop":
            drop_name(argv[1])
            return 0
        print(
            "usage: _shared_db.py create <label> | drop <name>  "
            "(HYDRA_TEST_DATABASE_URL required)",
            file=sys.stderr,
        )
        return 2
    except SetupError as e:
        print(f"[shared-db] {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(_main(sys.argv[1:]))
