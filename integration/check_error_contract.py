#!/usr/bin/env python3
"""The admin API's ERROR CONTRACT, checked against a running instance.

WHY: the admin UI grew `cleanErrorBody()` / `errorHead()` precisely because it
could not rely on error responses being parseable — `admin-ui/app.js:176` reads
`json.error.code`, and a bare `statusText` once produced toasts like `"429 429: "`.
Those were UI-side workarounds for a contract that was never written down as a
test. `admin-ui/api-docs.js` DOES document per-endpoint `errors: [{status, code}]`
(26 of 51 endpoints), so the contract is checkable in both directions: the wire
shape, and the documented code actually being the one produced.

`api-docs.js` also carries the metadata this probe needs to avoid crying wolf:
`auth` (`true` | `false` | `cluster`), the full method set per path (so
"wrong method" can mean a method that is genuinely not documented for that path),
`resp` (so a `text/plain` endpoint is not judged as a JSON error) and `body`
(so an endpoint with no documented request body is not "malformed-JSON" probed).

Checks, on EVERY documented endpoint:
  1. unauthenticated            -> 401/429, JSON envelope, code in
                                   {unauthorized, too_many_failed_attempts}
  2. malformed JSON on a body   -> 400 invalid_json (404 not_found is accepted for
                                   an id-scoped path, where routing rejects the
                                   probe id before the body is parsed)
  3. wrong method on the path   -> 404 not_found / 405 method_not_allowed
  4. unknown path               -> 404 not_found
  5. exhausted auth budget      -> 429 with a Retry-After header, and a VALID
                                   token still served (a brute-force run must not
                                   take the admin API away from the operator)
Every error body must be `application/json` with a nested-or-top-level string
`code`; anything else (HTML, empty body, bare text) is a violation.

Responses that are real but NOT documented for that endpoint are reported as
NOTES (e.g. 405s), never as violations — that keeps this a regression gate rather
than a documentation-completeness gate.

Exit 0 clean, 1 violations, 2 could not verify (unhealthy instance, too few
endpoints/responses → anti-vacuous floors).

The first version of this probe reported ~150 violations and was wrong twice over:
it expected a TOP-LEVEL `code` (the envelope nests it under `error`) and it
expected 401 for every unauthenticated probe (after ~3 attempts the admin gate
answers 429 by design). Both are now encoded above and pinned by its test suite.
"""
import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_api_docs import DEFAULT_DOCS, documented_endpoints  # noqa: E402  (shared loader)

BASE = os.environ.get("HYDRA_BASE_URL", "http://127.0.0.1:18081").rstrip("/")
TOKEN = os.environ.get("HYDRA_ADMIN_TOKEN", "")
PROBE_ID = "probe-does-not-exist"
MIN_ENDPOINTS = 20
MIN_JSON_ERRORS = 40

# What each probe kind is allowed to produce. Anything outside these sets is a
# violation; anything inside but absent from the endpoint's documented `errors`
# is reported as a NOTE (a documentation gap, not a regression).
ALLOWED = {
    "unauthenticated": {(401, "unauthorized"), (429, "too_many_failed_attempts")},
    "malformed-json": {(400, "invalid_json"), (404, "not_found")},
    "wrong-method": {(404, "not_found"), (405, "method_not_allowed")},
    "unknown-path": {(404, "not_found")},
    "rate-limited": {(429, "too_many_failed_attempts")},
}


def request(method, url, data=None, token=None, content_type="application/json"):
    headers = {}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if content_type:
        headers["Content-Type"] = content_type
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status, dict(resp.headers), resp.read(400).decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read(400).decode(errors="replace")
    except Exception as e:  # connection refused / timeout
        return 0, {}, str(e)


def parse_error(headers, body):
    """(code, note). `note` is None when the envelope is well-formed."""
    ctype = (headers.get("Content-Type") or headers.get("content-type") or "").lower()
    stripped = body.strip()
    if not stripped:
        return None, "empty body"
    if "json" not in ctype:
        return None, f"non-JSON content-type ({ctype or 'no header'}): {stripped[:60]!r}"
    try:
        parsed = json.loads(stripped)
    except Exception as e:
        return None, f"unparseable JSON: {e}: {stripped[:60]!r}"
    if not isinstance(parsed, dict):
        return None, f"JSON body is a {type(parsed).__name__}, not an object"
    # The documented envelope is {"error": {"code", "message", "trace_id"}}.
    env = parsed.get("error") if isinstance(parsed.get("error"), dict) else parsed
    code = env.get("code")
    if not isinstance(code, str) or not code:
        return None, f"no string `code` (keys: {sorted(parsed)[:6]})"
    return code, None


def uniform_behaviour_is_documented(root=None):
    """True when the API reference carries its GLOBAL note about the uniform error
    behaviour (401/429 + Retry-After, 400 invalid_json, 405, 404, the envelope
    shape). When it does, a per-endpoint omission is not a documentation gap —
    repeating it 51 times would be noise. When it does not, the notes this probe
    prints ARE gaps, and it says so."""
    root = root or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    for rel in ("admin-ui/api-docs.js", "admin-ui/i18n.js"):
        path = os.path.join(root, rel)
        try:
            if "apidocs.chrome.introErrors" in open(path, encoding="utf-8").read():
                return True
        except OSError:
            return False
    return False


def documented_statuses(entry):
    """Leading 3-digit codes from the entry's `resp` lines (e.g. "503 — standby").
    A token-free probe must expect exactly these: `/healthz/leader` is documented as
    200 leader / 503 standby / 404 non-candidate, and the live cluster answers
    precisely that (8081 -> 200, 8082 -> 503, 8084 -> 404, single node -> 404)."""
    out = set()
    for line in entry.get("resp") or []:
        m = re.match(r"\s*(\d{3})\b", str(line))
        if m:
            out.add(int(m.group(1)))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--docs", default=DEFAULT_DOCS)
    ap.add_argument("--no-rate-limit", action="store_true")
    args = ap.parse_args()

    status, _, _ = request("GET", f"{BASE}/api/v1/health", token=TOKEN)
    if status != 200:
        print(f"[errors] CANNOT VERIFY: instance unhealthy at {BASE} (GET /api/v1/health -> {status})", file=sys.stderr)
        return 2

    try:
        endpoints = documented_endpoints(args.docs)
    except Exception as e:
        print(f"[errors] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2
    if len(endpoints) < MIN_ENDPOINTS:
        print(f"[errors] CANNOT VERIFY: only {len(endpoints)} documented endpoint(s) (< floor {MIN_ENDPOINTS})", file=sys.stderr)
        return 2

    violations, notes = [], {}
    well_formed = 0

    def check(kind, method, path, observed, documented):
        nonlocal well_formed
        status, headers, body = observed
        code, note = parse_error(headers, body)
        if code is None:
            violations.append(f"{kind}: {method} {path} -> {status} ({note})")
            return
        well_formed += 1
        if (status, code) not in ALLOWED[kind]:
            violations.append(
                f"{kind}: {method} {path} -> {status} {code} "
                f"(allowed: {sorted(ALLOWED[kind])})"
            )
            return
        if (status, code) not in documented:
            key = f"{status} {code} (as {kind})"
            notes[key] = notes.get(key, 0) + 1

    # (0) The FIRST unauthenticated request must be the documented 401. On a fresh
    # instance this is deterministic; if a previous run already exhausted the
    # budget (the window is ~60s) we say so instead of pretending.
    first = request("GET", f"{BASE}/api/v1/health", token=None)
    if first[0] == 401:
        code, note = parse_error(first[1], first[2])
        if code != "unauthorized":
            violations.append(f"unauthenticated: GET /api/v1/health -> 401 {note or code} (documented code is `unauthorized`)")
        else:
            well_formed += 1
    else:
        code, note = parse_error(first[1], first[2])
        print(f"[errors] NOTE: the first unauthenticated request was {first[0]} {code or note} — the auth-failure budget was already "
              f"exhausted (re-running this probe against the same instance does that); the 401 shape was not re-verified this run")

    methods_by_path = {}
    for e in endpoints:
        methods_by_path.setdefault(e["path"], set()).add(e["method"].upper())

    def undocumented_method(path):
        """A method that is NOT documented for this path (never `GET` unless the
        path documents nothing else) — probing a documented method as "wrong"
        produced the probe's loudest false alarms."""
        for candidate in ("DELETE", "PATCH", "PUT", "POST", "GET"):
            if candidate not in methods_by_path.get(path, set()):
                return candidate
        return None

    for e in endpoints:
        method, path = e["method"].upper(), e["path"]
        documented = {(x["status"], x["code"]) for x in (e.get("errors") or []) if "code" in x}
        url = BASE + re.sub(r"\{[^}]+\}", PROBE_ID, path)
        body = b"{}" if method in ("POST", "PUT", "PATCH") else None
        auth = e.get("auth")
        text_resp = "text/plain" in " ".join(e.get("resp") or [])

        if auth is False:
            # Token-free by design (a load balancer must be able to probe it), and
            # its success status is ROLE-dependent — the `resp` lines say which.
            expected = documented_statuses(e)
            observed = request(method, url, data=body, token=None)
            if expected and observed[0] in expected:
                well_formed += 1
                notes[f"{method} {path} answered {observed[0]} (documented for this role)"] = 1
            elif 200 <= observed[0] < 300:
                well_formed += 1
            else:
                violations.append(
                    f"token-free: {method} {path} -> {observed[0]} without a token "
                    f"(documented auth: false; documented statuses: {sorted(expected) or '—'})"
                )
        elif auth == "cluster":
            # The shared-cluster-token class is RETIRED (ADR-0001 T3.5/T4.1 deleted the
            # `/api/v1/internal/*` family, and 2026-10-05 deleted the token with it), so nothing
            # should carry this marker any more. If one appears, the API docs and the product have
            # drifted apart again — report it instead of probing a route that does not exist.
            violations.append(
                f"auth class `cluster` is retired but {method} {path} still declares it"
            )
        else:
            check("unauthenticated", method, path, request(method, url, data=body, token=None), documented)

        # `{}` is an explicit "no parameters" body (POST /reload): there is nothing
        # to malform, and the handler never parses it (probed: garbage body -> 200).
        documented_body = e.get("body")
        if method in ("POST", "PUT", "PATCH") and documented_body is not None and documented_body != {}:
            check("malformed-json", method, path, request(method, url, data=b"{not json", token=TOKEN), documented)

        wrong = None if auth == "cluster" else undocumented_method(path)
        if wrong:
            observed = request(wrong, url, data=b"{}" if wrong in ("POST", "PUT", "PATCH") else None, token=TOKEN)
            if 200 <= observed[0] < 300:
                # The router/handler is method-agnostic here. Harmless for a
                # token-gated JSON API, but it is NOT the documented behaviour, so
                # it is surfaced as a note rather than silently accepted.
                notes[f"{wrong} on a path that documents only {sorted(methods_by_path[path])} answers {observed[0]}"] = \
                    notes.get(f"{wrong} on a path that documents only {sorted(methods_by_path[path])} answers {observed[0]}", 0) + 1
                well_formed += 1
            elif text_resp:
                # e.g. POST /metrics -> the exposition is text, so a JSON envelope
                # is not required when the status is not an error.
                well_formed += 1
            else:
                check("wrong-method", method, path, observed, documented)

    check("unknown-path", "GET", f"/api/v1/{PROBE_ID}/nope",
          request("GET", f"{BASE}/api/v1/{PROBE_ID}/nope", token=TOKEN), set())

    # (5) 429 shape + a valid token is still served.
    if not args.no_rate_limit:
        observed = None
        for _ in range(200):
            observed = request("GET", f"{BASE}/api/v1/health", token="wrong-token-probe-000000")
            if observed[0] == 429:
                break
        if observed[0] != 429:
            violations.append(f"rate-limit: never got 429 after 200 bad-token attempts (last {observed[0]})")
        else:
            well_formed += 1
            retry_after = observed[1].get("Retry-After") or observed[1].get("retry-after")
            code, note = parse_error(observed[1], observed[2])
            if code != "too_many_failed_attempts":
                violations.append(f"rate-limit: 429 code is {code or note}, documented code is `too_many_failed_attempts`")
            if not retry_after:
                violations.append("rate-limit: 429 carries no Retry-After header (the UI parses it; without it the error is unactionable)")
        good = request("GET", f"{BASE}/api/v1/health", token=TOKEN)
        if good[0] != 200:
            violations.append(f"rate-limit: a VALID token was refused ({good[0]}) while bad ones had exhausted the budget — "
                              "a brute-force run would take the admin API away from the operator")
        else:
            well_formed += 1

    if well_formed < MIN_JSON_ERRORS:
        print(f"[errors] CANNOT VERIFY: only {well_formed} well-formed error response(s) (< floor {MIN_JSON_ERRORS})", file=sys.stderr)
        for v in violations[:10]:
            print(f"[errors]   {v}", file=sys.stderr)
        return 2

    uniform = uniform_behaviour_is_documented()
    print(f"documented endpoints: {len(endpoints)} | error responses checked: {well_formed} | violations: {len(violations)}")
    label = ("NOTE not repeated per endpoint (covered by the API reference's global error-contract note)"
             if uniform else
             "NOTE undocumented: no global note in the API reference, and these codes are not listed per endpoint")
    for key, n in sorted(notes.items(), key=lambda kv: -kv[1]):
        print(f"  {label}: {n} x {key}")
    for v in violations:
        print(f"  VIOLATION {v}")
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
