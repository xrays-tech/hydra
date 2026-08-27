import { Command } from 'commander';
import type { EffectiveOpts, GlobalOpts } from '../config.js';

/** Attach the four global options to a command (used on the root + every leaf). */
export function addGlobalOptions(cmd: Command): Command {
  cmd
    .option('--base-url <url>', 'Hydra base URL (env: HYDRA_BASE_URL, HYDRA_HOST)')
    .option('--token <tok>', 'Admin bearer token (env: HYDRA_ADMIN_TOKEN)')
    .option('--json', 'Raw JSON output, skip table formatting')
    .option('-v, --verbose', 'Print HTTP method + URL to stderr');
  return cmd;
}

function ancestors(cmd: Command): Command[] {
  const chain: Command[] = [];
  let c: Command | null = cmd;
  while (c) {
    chain.unshift(c);
    c = c.parent;
  }
  return chain;
}

/**
 * Merge global options from every ancestor command with the leaf command's
 * options. Leaf values win, but undefined leaf values never clobber a value
 * supplied on a parent (so `--token X cmd sub` and `cmd sub --token X` both
 * work, as do options on intermediate command groups).
 */
export function effectiveOpts(cmd: Command): EffectiveOpts {
  const merged: Record<string, unknown> = {};
  for (const c of ancestors(cmd)) {
    for (const [k, v] of Object.entries(c.opts() as Record<string, unknown>)) {
      if (v !== undefined) merged[k] = v;
    }
  }
  return merged as EffectiveOpts;
}

type AsyncAction = (...args: unknown[]) => Promise<void> | void;

/**
 * Wrap an async commander action so any thrown Error is reported as
 * `Error: <message>` on stderr and the process exits with status 1.
 */
export function withErrorHandler(fn: AsyncAction): AsyncAction {
  return async (...args: unknown[]) => {
    try {
      await fn(...args);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      console.error(`Error: ${msg}`);
      process.exit(1);
    }
  };
}
