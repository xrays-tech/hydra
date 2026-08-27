import { Command } from 'commander';
import pkg from '../package.json' with { type: 'json' };
import { addGlobalOptions } from './commands/shared.js';
import { buildHealthCommand } from './commands/health.js';
import { buildReloadCommand } from './commands/reload.js';
import { buildMetricsCommand } from './commands/metrics.js';
import { buildConcurrencyCommand } from './commands/concurrency.js';
import { buildEntityCommand } from './commands/entities.js';
import {
  buildAuthCacheCommand,
  buildBreakerCommand,
  buildClusterCommand,
  buildStatsCommand,
  buildTenantAuthTestCommand,
} from './commands/system.js';
import { ENTITY_DEFS } from './types.js';

const program = new Command();

program
  .name('hydra-admin')
  .description(
    'Command-line client for the Hydra LLM gateway admin REST API.\n\n' +
      'Global options may be given before or after the subcommand and can also\n' +
      'be supplied via environment variables (HYDRA_BASE_URL / HYDRA_HOST,\n' +
      'HYDRA_ADMIN_TOKEN).',
  )
  .version(pkg.version);

// Global options live on the root so `hydra-admin --token X <cmd> ...` works.
addGlobalOptions(program);

// Standalone endpoints.
program.addCommand(buildHealthCommand());
program.addCommand(buildReloadCommand());
program.addCommand(buildMetricsCommand());
program.addCommand(buildConcurrencyCommand());
program.addCommand(buildBreakerCommand());
program.addCommand(buildAuthCacheCommand());
program.addCommand(buildStatsCommand());
program.addCommand(buildClusterCommand());

// CRUD entity groups, all produced by one generic factory.
for (const def of ENTITY_DEFS) {
  const cmd = buildEntityCommand(def);
  if (def.command === 'tenants') {
    cmd.addCommand(buildTenantAuthTestCommand());
  }
  program.addCommand(cmd);
}

/**
 * Flush stdout then exit with the given code. Node's global `fetch` (undici)
 * keeps keep-alive sockets open, which would otherwise hold the event loop
 * alive after a successful request and stop the CLI from returning to the
 * shell. Flushing first guarantees large outputs (e.g. `metrics`) aren't
 * truncated.
 */
function finish(code: number): void {
  process.exitCode = code;
  process.stdout.write('', () => process.exit(code));
}

program
  .parseAsync(process.argv)
  .then(() => finish(0))
  .catch((e: unknown) => {
    const msg = e instanceof Error ? e.message : String(e);
    console.error(`Error: ${msg}`);
    finish(1);
  });
