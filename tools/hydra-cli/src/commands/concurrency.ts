import { Command } from 'commander';
import { HydraClient } from '../client.js';
import { resolveConfig } from '../config.js';
import { printJson, printTable } from '../format.js';
import { addGlobalOptions, effectiveOpts, withErrorHandler } from './shared.js';

const CONCURRENCY_COLUMNS = [
  { field: 'provider_id', header: 'PROVIDER', width: 18 },
  { field: 'gated', header: 'GATED', width: 7 },
  { field: 'max_concurrency', header: 'MAX', width: 7 },
  { field: 'inflight', header: 'INFLIGHT', width: 9 },
  { field: 'available', header: 'AVAILABLE', width: 10 },
  { field: 'queue_depth', header: 'QUEUE', width: 7 },
];

export function buildConcurrencyCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('concurrency').description('Show live admission / concurrency state.'),
  );
  cmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = await client.concurrency();
      if (opts.json) {
        printJson(res);
        return;
      }
      const rows = Array.isArray(res)
        ? (res as Array<Record<string, unknown>>)
        : Array.isArray((res as Record<string, unknown> | null)?.providers)
          ? ((res as Record<string, unknown>).providers as Array<Record<string, unknown>>)
          : [];
      printTable(CONCURRENCY_COLUMNS, rows);
    }),
  );
  return cmd;
}
