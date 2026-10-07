#!/usr/bin/env python3
"""The ONE owner of "which usage backend a drill's node is started with" (ADR-0002 T2.7).

Why this file exists: `HYDRA_USAGE_SINK` is becoming a REQUIRED setting (ADR-0002 D-1: the process
refuses to start without it, because "where does the billing data go" is a decision rather than a
guess). Before this helper there was no owner at all — each drill built its own `env` dictionary
inline, and **27 of them never set the variable**, relying on the compiled-in default. That is the
same shape as the six cluster variables the startup-knobs drill had to be re-pointed at: the
foundation moved and every drill had its own copy of the assumption.

So the drills ask this module instead, and the answer is stated once:

* `usage_env()` — what a drill's node gets **unless it says otherwise**: usage switched OFF
  (`none`). A drill that does not read usage rows should not make the node write them, and it should
  not point the sink at a ClickHouse that is not running either: the `none` backend costs nothing,
  logs nothing per request and, being explicit, does not look like a misconfiguration.
* `usage_env("clickhouse", url)` — for a drill that asserts what was WRITTEN. Pair it with
  `_mock_clickhouse.py`, which is a real HTTP server that records the rows, so "the row landed" is
  still a measurement rather than an inspection of a log line.
* `usage_env("clickhouse", url, dead=True)` — for a drill about the failure path (the drop counter,
  the retry/backoff): a URL nothing answers on.

Nothing here is a mock of Hydra itself: the drills still start the real binary, and a drill that
asserts usage rows still goes through the real writer, the real transport and a real HTTP server.
"""

from __future__ import annotations

import os

# The value a drill's node gets unless it asks for something else.
#
# `none`, because SQLite is retired (ADR-0002 D-3) and a drill that does not read usage should not
# make the node write it — nor point a sink at a ClickHouse that is not running, which is what the
# twelve drills with a dead URL as a boot dependency have to do. A drill that ASSERTS usage rows
# asks for `clickhouse` explicitly and pairs it with `_mock_clickhouse.py`.
DEFAULT_KIND = "none"


def usage_env(kind: str | None = None, url: str | None = None, *, dead: bool = False) -> dict:
    """The usage settings a drill's node should be started with.

    `kind="clickhouse"` requires `url` (or `dead=True`, which invents a URL on a closed port: the
    drill is then about the failure path, so nothing is expected to answer).
    """
    kind = kind or DEFAULT_KIND
    if kind == "clickhouse":
        if url is None:
            if not dead:
                raise ValueError(
                    "a drill that starts the ClickHouse usage backend must pass the URL of a real "
                    "server (see integration/_mock_clickhouse.py) — or dead=True to exercise the "
                    "failure path deliberately"
                )
            url = "http://127.0.0.1:9/usage-drill-nothing-listens-here"
        env = {"HYDRA_USAGE_SINK": "clickhouse", "HYDRA_CLICKHOUSE_URL": url}
    else:
        env = {"HYDRA_USAGE_SINK": kind}
    if dead:
        # A dead backend is the point of the drill: make sure a stray URL from the environment does
        # not accidentally point the node at something that answers.
        env["HYDRA_CLICKHOUSE_URL"] = url or "http://127.0.0.1:9/usage-drill-nothing-listens-here"
    return env


def apply(env: dict) -> dict:
    """Merge the drill defaults into `env`, WITHOUT overwriting what the drill already set.

    Called right after a drill builds `env = dict(os.environ)`, so a drill that pins a value keeps
    pinning it and the rest get the one owner's answer.
    """
    for key, value in usage_env().items():
        env.setdefault(key, value)
    return env


def has_usage_setting() -> bool:
    """True when the surrounding environment already names a usage backend (CI sets none today)."""
    return bool(os.environ.get("HYDRA_USAGE_SINK"))
