import { Command } from 'commander';
import { HydraClient } from '../client.js';
import { resolveConfig } from '../config.js';
import { printJson, printNotice, printRaw, printSuccess, printTable } from '../format.js';
import { addGlobalOptions, effectiveOpts, withErrorHandler } from './shared.js';

const BREAKER_COLUMNS = [
  { field: 'provider_id', header: 'PROVIDER', width: 18 },
  { field: 'state', header: 'STATE', width: 8 },
];

const USAGE_ROW_COLUMNS = [
  { field: 'name', header: 'NAME', width: 24 },
  { field: 'requests', header: 'REQUESTS', width: 12 },
  { field: 'tokens', header: 'TOKENS', width: 12 },
  { field: 'tokens_prompt', header: 'PROMPT', width: 12 },
  { field: 'tokens_completion', header: 'COMPLETION', width: 12 },
];

const CLUSTER_NODE_COLUMNS = [
  { field: 'node_id', header: 'NODE', width: 18 },
  { field: 'role', header: 'ROLE', width: 10 },
  { field: 'control_url', header: 'CONTROL_URL', width: 36 },
  { field: 'alive', header: 'ALIVE', width: 7 },
  { field: 'is_lease_holder', header: 'LEADER', width: 7 },
  { field: 'is_self', header: 'SELF', width: 6 },
];

/** Build `hydra-admin breaker` — inspect/reset the circuit-breaker dead set. */
export function buildBreakerCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('breaker').description('Inspect or reset circuit-breaker state.'),
  );

  const listCmd = addGlobalOptions(
    cmd
      .command('list', { isDefault: true })
      .description('List providers currently excluded by the circuit breaker.'),
  );
  listCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = (await client.breakerList()) as Record<string, unknown> | null;
      if (opts.json) {
        printJson(res);
        return;
      }
      const dead = Array.isArray(res?.dead) ? (res.dead as unknown[]) : [];
      if (dead.length === 0) {
        printNotice('no dead providers');
        return;
      }
      printTable(
        BREAKER_COLUMNS,
        dead.map((providerId) => ({
          provider_id: String(providerId),
          state: 'DEAD',
        })),
      );
    }),
  );

  const resetCmd = addGlobalOptions(
    cmd
      .command('reset <provider-id>')
      .description('Force-clear the circuit breaker for one provider.'),
  );
  resetCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const providerId = String(args[0]);
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = await client.breakerReset(providerId);
      if (opts.json) {
        printJson(res);
        return;
      }
      printSuccess(`breaker reset for ${providerId}`);
    }),
  );

  return cmd;
}

/** Build `hydra-admin auth-cache` — admin-token auth-cache invalidation. */
export function buildAuthCacheCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('auth-cache').description('Invalidate cached auth decisions.'),
  );
  const invalidateCmd = addGlobalOptions(
    cmd
      .command('invalidate', { isDefault: true })
      .description('Invalidate auth-cache entries (by tenant, api-keys, or all).'),
  );
  invalidateCmd
    .option('--tenant-id <id>', 'Only invalidate entries for this tenant')
    .option('--api-key <key>', 'Invalidate a specific client api-key (repeatable)', collect, []);
  invalidateCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const body: Record<string, unknown> = {};
      if (opts.tenantId !== undefined) body['tenant_id'] = opts.tenantId;
      if (Array.isArray(opts.apiKey) && opts.apiKey.length > 0) body['api_keys'] = opts.apiKey;
      const res = (await client.authCacheInvalidate(body)) as Record<string, unknown> | null;
      if (opts.json) {
        printJson(res);
        return;
      }
      const count = res && typeof res['invalidated'] === 'number' ? res['invalidated'] : 0;
      printSuccess(`invalidated ${count} auth-cache entr${count === 1 ? 'y' : 'ies'}`);
    }),
  );
  return cmd;
}

/** Build `hydra-admin stats` — aggregated usage statistics. */
export function buildStatsCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('stats').description('Show aggregated usage statistics.'),
  );
  const usageCmd = addGlobalOptions(
    cmd
      .command('usage', { isDefault: true })
      .description('Show usage totals broken down by tenant and provider.'),
  );
  usageCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = (await client.statsUsage()) as Record<string, unknown> | null;
      if (opts.json) {
        printJson(res);
        return;
      }
      if (!res || typeof res !== 'object') {
        printRaw('(no usage data)');
        return;
      }
      const totals = res['totals'] as Record<string, unknown> | undefined;
      if (totals) {
        console.log(
          `Totals: ${String(totals['requests'] ?? 0)} requests, ${String(totals['tokens'] ?? 0)} tokens ` +
            `(${String(totals['tokens_prompt'] ?? 0)} prompt / ${String(totals['tokens_completion'] ?? 0)} completion)`,
        );
        console.log('');
      }
      const byTenant = Array.isArray(res['by_tenant']) ? res['by_tenant'] as Array<Record<string, unknown>> : [];
      const byProvider = Array.isArray(res['by_provider']) ? res['by_provider'] as Array<Record<string, unknown>> : [];
      console.log('By tenant:');
      printTable(USAGE_ROW_COLUMNS, byTenant);
      console.log('By provider:');
      printTable(USAGE_ROW_COLUMNS, byProvider);
    }),
  );
  return cmd;
}

/** Build `hydra-admin cluster` — whole-cluster fleet status. */
export function buildClusterCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('cluster').description('Inspect cluster fleet status.'),
  );
  const statusCmd = addGlobalOptions(
    cmd
      .command('status', { isDefault: true })
      .description('Show cluster status and fleet nodes.'),
  );
  statusCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = (await client.clusterStatus()) as Record<string, unknown> | null;
      if (opts.json) {
        printJson(res);
        return;
      }
      if (!res || typeof res !== 'object') {
        printRaw('(no cluster status)');
        return;
      }
      if (res['cluster'] === false) {
        printNotice(`single-node mode (${String(res['mode'] ?? 'single')})`);
        return;
      }
      const nodes = Array.isArray(res['nodes']) ? res['nodes'] as Array<Record<string, unknown>> : [];
      printTable(CLUSTER_NODE_COLUMNS, nodes);
      const leaseHolder = res['lease_holder'] == null ? '' : String(res['lease_holder']);
      console.log(`lease holder: ${leaseHolder || '(none)'}`);
    }),
  );
  return cmd;
}

/** Build the `tenants auth-test` subcommand. */
export function buildTenantAuthTestCommand(): Command {
  const cmd = addGlobalOptions(
    new Command('auth-test <auth-url>')
      .description('Probe a tenant auth URL with a fake api-key.'),
  );
  cmd.option('--tenant-id <id>', 'Tenant id sent with the simulated auth request');
  cmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const authUrl = String(args[0]);
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const body: Record<string, unknown> = { auth_url: authUrl };
      if (opts.tenantId !== undefined) body['tenant_id'] = opts.tenantId;
      const res = (await client.tenantAuthTest(body)) as Record<string, unknown> | null;
      if (opts.json) {
        printJson(res);
        return;
      }
      if (!res || typeof res !== 'object') {
        printRaw('(no result)');
        return;
      }
      const ok = res['ok'] === true;
      console.log(
        `${ok ? '✓' : '✗'} verdict=${String(res['verdict'] ?? 'unknown')} ` +
          `reachable=${String(res['reachable'] ?? false)} status=${String(res['status'] ?? '—')} ` +
          `duration_ms=${String(res['duration_ms'] ?? '—')}`,
      );
      if (res['detail']) console.log(String(res['detail']));
    }),
  );
  return cmd;
}

function collect(value: string, previous: string[]): string[] {
  return [...previous, value];
}
