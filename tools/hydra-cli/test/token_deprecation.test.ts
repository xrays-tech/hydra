/**
 * Tests for the `--token` deprecation warning (decision 2026-10-09).
 *
 * A token passed on the command line is visible in the process listing (`ps`)
 * and in shell history — a live admin credential leak. The flag is DEPRECATED,
 * not removed: it still works (flags win over env), and existing scripts plus
 * `integration/test_cli_live.py` depend on it. The only change is that a
 * non-empty `--token` now prints a warning to stderr; a token from
 * `HYDRA_ADMIN_TOKEN` stays quiet.
 *
 * These tests pin both directions:
 *   1. flag-supplied token  -> warning on stderr (and the token is still used);
 *   2. env-supplied token   -> no warning.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { resolveConfig, TOKEN_DEPRECATION_WARNING } from '../src/config.js';

/**
 * Capture everything written to process.stderr for the duration of `fn`.
 * `resolveConfig` writes the warning via `process.stderr.write` directly, so
 * replacing the stream's `write` is a deterministic in-process capture (no
 * subprocess needed).
 */
function captureStderr(fn: () => unknown): string {
  const original = process.stderr.write.bind(process.stderr);
  const chunks: string[] = [];
  // We intentionally swap the stream's writer under the call.
  process.stderr.write = (chunk: unknown) => {
    chunks.push(String(chunk));
    return true;
  };
  try {
    fn();
  } finally {
    process.stderr.write = original;
  }
  return chunks.join('');
}

describe('resolveConfig: --token deprecation warning', () => {
  it('warns on stderr when the token comes from the --token flag', () => {
    delete process.env.HYDRA_ADMIN_TOKEN;
    let cfg: ReturnType<typeof resolveConfig> | undefined;
    const out = captureStderr(() => {
      cfg = resolveConfig({ token: 'flagtok' });
    });
    assert.ok(out.includes('deprecated'), `expected a deprecation warning, got: ${out}`);
    assert.ok(out.includes('HYDRA_ADMIN_TOKEN'), `the warning must name the env var, got: ${out}`);
    assert.equal(out.trim(), TOKEN_DEPRECATION_WARNING, 'the warning is one line, unmodified');
    // The flag still WORKS: its value is what gets used.
    assert.equal(cfg?.token, 'flagtok');
  });
  it('does not warn when the token comes from HYDRA_ADMIN_TOKEN', () => {
    process.env.HYDRA_ADMIN_TOKEN = 'envtok';
    let cfg: ReturnType<typeof resolveConfig> | undefined;
    const out = captureStderr(() => {
      cfg = resolveConfig({});
    });
    assert.ok(!out.includes('deprecated'), `env-supplied token must be quiet, got: ${out}`);
    assert.equal(cfg?.token, 'envtok');
    delete process.env.HYDRA_ADMIN_TOKEN;
  });
  it('does not warn for an empty --token (nothing is being leaked)', () => {
    delete process.env.HYDRA_ADMIN_TOKEN;
    const out = captureStderr(() => {
      // No env either: resolveConfig must still fail, but without a warning —
      // an empty flag value was never a token.
      assert.throws(() => resolveConfig({ token: '' }));
    });
    assert.ok(!out.includes('deprecated'), `empty flag must not warn, got: ${out}`);
  });
});
