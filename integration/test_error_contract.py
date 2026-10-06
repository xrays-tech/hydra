#!/usr/bin/env python3
"""Unit tests for the error-contract probe's pure helpers.

Why this file exists: `integration/check_error_contract.py` reported **21
"violations"** on a healthy instance the first time it ran, and every single one
was its own bug —
  1. it expected a TOP-LEVEL `code` while the documented envelope nests it under
     `error`;
  2. it expected 401 for every unauthenticated probe, but the admin gate answers
     429 (`too_many_failed_attempts`) once its per-peer budget trips;
  3. it probed "a wrong method" on paths that document several methods (so
     `GET /api/v1/providers` was flagged for returning the provider list);
  4. the shared loader copied only `{method, path, body}`, so the whole
     documented-code cross-check was vacuous while still printing "undocumented"
     for everything.
These tests pin the first three (the fourth is pinned by asserting the loader's
output shape); the end-to-end behaviour is exercised by
`integration/run-crud-local.sh`, which runs the probe against a live instance.

Run: python3 integration/test_error_contract.py
"""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check_error_contract as probe  # noqa: E402


class ParseErrorTests(unittest.TestCase):
    def test_documented_nested_envelope(self):
        body = json.dumps({"error": {"code": "not_found", "message": "no such id", "trace_id": "hydra-abc-1"}})
        code, note = probe.parse_error({"Content-Type": "application/json"}, body)
        self.assertEqual(code, "not_found")
        self.assertIsNone(note)

    def test_top_level_code_is_also_accepted(self):
        code, note = probe.parse_error({"Content-Type": "application/json"}, '{"code":"invalid_json"}')
        self.assertEqual(code, "invalid_json")
        self.assertIsNone(note)

    def test_empty_body_is_a_violation_not_a_pass(self):
        code, note = probe.parse_error({"Content-Type": "application/json"}, "   ")
        self.assertIsNone(code)
        self.assertIn("empty body", note)

    def test_html_error_page_is_a_violation(self):
        code, note = probe.parse_error({"Content-Type": "text/html"}, "<html><title>502 Bad Gateway</title></html>")
        self.assertIsNone(code)
        self.assertIn("non-JSON content-type", note)

    def test_wrong_content_type_with_json_body_still_reported(self):
        code, note = probe.parse_error({"Content-Type": "text/plain"}, '{"error":{"code":"unauthorized"}}')
        self.assertIsNone(code)
        self.assertIn("non-JSON content-type", note)

    def test_unparseable_json_is_a_violation(self):
        code, note = probe.parse_error({"Content-Type": "application/json"}, "{oops")
        self.assertIsNone(code)
        self.assertIn("unparseable JSON", note)

    def test_json_array_body_is_a_violation(self):
        # A list is a SUCCESS shape here (the CRUD list endpoints), never an error.
        code, note = probe.parse_error({"Content-Type": "application/json"}, "[1,2]")
        self.assertIsNone(code)
        self.assertIn("not an object", note)

    def test_missing_code_is_a_violation(self):
        code, note = probe.parse_error({"Content-Type": "application/json"}, '{"error":{"message":"nope"}}')
        self.assertIsNone(code)
        self.assertIn("no string `code`", note)

    def test_header_lookup_is_case_insensitive(self):
        code, _ = probe.parse_error({"content-type": "application/json; charset=utf-8"}, '{"error":{"code":"unauthorized"}}')
        self.assertEqual(code, "unauthorized")


class DocumentedStatusTests(unittest.TestCase):
    def test_role_dependent_statuses(self):
        entry = {"resp": ["200 — this node is the active leader", "503 — standby (not the lease holder)",
                          "404 — non-candidate / single-node"]}
        self.assertEqual(probe.documented_statuses(entry), {200, 503, 404})

    def test_text_responses_have_their_status_parsed(self):
        self.assertEqual(probe.documented_statuses({"resp": ["200 text/plain — Prometheus exposition (0.0.4)"]}), {200})

    def test_entries_without_status_codes_yield_nothing(self):
        self.assertEqual(probe.documented_statuses({"resp": ["whatever"]}), set())
        self.assertEqual(probe.documented_statuses({}), set())


class AllowedSetTests(unittest.TestCase):
    def test_every_probe_kind_has_an_expectation(self):
        for kind in ("unauthenticated", "malformed-json", "wrong-method", "unknown-path", "rate-limited"):
            self.assertIn(kind, probe.ALLOWED, kind)
            self.assertTrue(probe.ALLOWED[kind])

    def test_the_documented_codes_are_the_expected_ones(self):
        self.assertIn((401, "unauthorized"), probe.ALLOWED["unauthenticated"])
        self.assertIn((429, "too_many_failed_attempts"), probe.ALLOWED["unauthenticated"])
        self.assertEqual(probe.ALLOWED["malformed-json"], {(400, "invalid_json"), (404, "not_found")})
        self.assertIn((405, "method_not_allowed"), probe.ALLOWED["wrong-method"])


class UniformNoteTests(unittest.TestCase):
    """Per-endpoint omissions are only acceptable because the API reference
    carries a GLOBAL note about the uniform error behaviour."""

    def test_the_shipped_reference_has_the_global_note(self):
        self.assertTrue(probe.uniform_behaviour_is_documented())

    def test_a_tree_without_the_note_is_detected(self):
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "admin-ui"))
            open(os.path.join(d, "admin-ui", "api-docs.js"), "w").close()
            open(os.path.join(d, "admin-ui", "i18n.js"), "w").close()
            self.assertFalse(probe.uniform_behaviour_is_documented(d))


class LoaderShapeTests(unittest.TestCase):
    """The metadata must reach the probe: without `auth`/`resp`/`errors` the
    cross-check silently verifies nothing (that was bug #4)."""

    @classmethod
    def setUpClass(cls):
        from check_api_docs import documented_endpoints
        docs = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "admin-ui", "api-docs.js")
        cls.endpoints = documented_endpoints(docs)

    def test_endpoints_carry_the_metadata(self):
        self.assertGreater(len(self.endpoints), 40)
        for e in self.endpoints:
            self.assertEqual(set(e), {"method", "path", "body", "auth", "resp", "errors"}, e.get("path"))

    def test_token_free_and_cluster_endpoints_are_visible(self):
        auths = {str(e["auth"]) for e in self.endpoints}
        self.assertIn("False", auths)   # /healthz/leader
        self.assertIn("cluster", auths)  # the internal control plane
        self.assertIn("True", auths)

    def test_documented_error_codes_are_visible(self):
        codes = {(x["status"], x["code"]) for e in self.endpoints for x in e["errors"]}
        self.assertIn((401, "unauthorized"), codes)
        self.assertIn((400, "invalid_json"), codes)
        self.assertIn((404, "not_found"), codes)

    def test_unknown_method_detection_uses_the_documented_method_set(self):
        # `undocumented_method` lives inside main(); the same rule is asserted
        # here through the documented method sets themselves.
        by_path = {}
        for e in self.endpoints:
            by_path.setdefault(e["path"], set()).add(e["method"].upper())
        self.assertGreaterEqual(by_path["/api/v1/providers"], {"GET", "POST"})
        self.assertIn("DELETE", by_path["/api/v1/providers/{id}"])
        # ...so "GET" is NOT a wrong method for /api/v1/providers, which is exactly
        # the false alarm the first version produced (it printed the DOCUMENTED
        # method in the message, making it look like POST had returned the list).
        self.assertIn("GET", by_path["/api/v1/providers"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
