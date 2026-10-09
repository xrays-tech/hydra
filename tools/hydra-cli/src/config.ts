/**
 * Resolves effective CLI configuration from flags + environment.
 *
 * Global options (available on every command):
 *   --base-url <url>   (env: HYDRA_BASE_URL / HYDRA_HOST)
 *   --token <tok>      DEPRECATED — use HYDRA_ADMIN_TOKEN instead (a token on argv
 *                      shows up in `ps` / shell history; the env var does not)
 *   --json             raw JSON output, skip table formatting
 *   -v, --verbose      print HTTP method + URL to stderr
 */

const DEFAULT_BASE_URL = 'http://127.0.0.1:8081';

export interface GlobalOpts {
  baseUrl?: string;
  token?: string;
  json?: boolean;
  verbose?: boolean;
}

export interface HydraConfig {
  baseUrl: string;
  token: string;
  json: boolean;
  verbose: boolean;
}

/** Merged options: global options plus any command-specific ones (record index). */
export type EffectiveOpts = GlobalOpts & Record<string, unknown>;

export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'ConfigError';
  }
}

/** Warn once per process (not per command invocation) that `--token` was used. */
let warnedTokenDeprecated = false;

/** The exact one-line stderr warning for `--token` (tested verbatim). */
export const TOKEN_DEPRECATION_WARNING =
  '[warn] --token is deprecated: it leaks the token into the process list and shell history. ' +
  'Use HYDRA_ADMIN_TOKEN instead.';

/**
 * Resolve final configuration. Precedence: explicit flag > env var > default.
 * Throws ConfigError (caught by the action wrapper) if the token is missing.
 */
export function resolveConfig(opts: GlobalOpts): HydraConfig {
  const rawBase =
    opts.baseUrl ??
    process.env.HYDRA_BASE_URL ??
    process.env.HYDRA_HOST ??
    DEFAULT_BASE_URL;
  const baseUrl = rawBase.replace(/\/+$/, '');

  // P3-6 (2026-10-09): `--token` still works (removing it would break existing
  // scripts and the live integration drill), but it is deprecated: a token on
  // argv appears in `ps` output and the shell history of a shared host. The env
  // var does not. `opts.token` is only ever set by the `--token` flag — the env
  // var is read separately below — so its presence identifies the deprecated form.
  if (opts.token && !warnedTokenDeprecated) {
    warnedTokenDeprecated = true;
    console.error(TOKEN_DEPRECATION_WARNING);
  }

  const token = (opts.token ?? process.env.HYDRA_ADMIN_TOKEN ?? '').trim();
  if (!token) {
    throw new ConfigError(
      'Admin token is required. Set HYDRA_ADMIN_TOKEN (or pass --token, which is deprecated).',
    );
  }

  return {
    baseUrl,
    token,
    json: opts.json ?? false,
    verbose: opts.verbose ?? false,
  };
}
