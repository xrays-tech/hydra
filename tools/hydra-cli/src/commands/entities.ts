import { Command, InvalidArgumentError } from 'commander';
import readline from 'node:readline/promises';
import { stdin as input, stdout as output } from 'node:process';
import { HydraClient } from '../client.js';
import { resolveConfig, type EffectiveOpts } from '../config.js';
import { printJson, printTable, printSuccess } from '../format.js';
import type { EntityDef, FieldDef } from '../types.js';
import { addGlobalOptions, effectiveOpts, withErrorHandler } from './shared.js';

/** Convert a flag spec like '--max-concurrency <n>' into its camelCase key. */
function optionKey(flag: string): string {
  const name = flag.replace(/^--/, '').replace(/\s+.*/, '');
  return name.replace(/-([a-z])/g, (_m, c: string) => c.toUpperCase());
}

function parseNumber(value: string, nullable?: boolean): number | null {
  if (nullable && value.toLowerCase() === 'null') return null;
  const n = Number(value);
  if (!Number.isFinite(n)) {
    throw new InvalidArgumentError(`expected a number, got "${value}"`);
  }
  return n;
}

/**
 * Build the JSON body for create/update from parsed options.
 *
 * Some server entities auto-fill created_at/updated_at when blank; for those
 * we send "" so callers never have to. Entities without timestamps (or with
 * only created_at) are handled by `def.timestamps`.
 *
 * @param isCreate when true, defaults are applied for omitted fields.
 */
function buildBody(
  def: EntityDef,
  opts: EffectiveOpts,
  isCreate: boolean,
): Record<string, unknown> {
  const body: Record<string, unknown> = {};

  for (const f of def.fields) {
    if (f.kind === 'boolean') {
      const trueKey = optionKey(f.flag);
      const falseKey = f.falseFlag ? optionKey(f.falseFlag) : '';
      if (opts[trueKey] === true) body[f.field] = true;
      else if (falseKey && opts[falseKey] === true) body[f.field] = false;
      else if (isCreate && f.default !== undefined) body[f.field] = f.default;
      continue;
    }

    const key = optionKey(f.flag);
    if (opts[key] !== undefined) {
      body[f.field] = opts[key];
    } else if (isCreate && f.default !== undefined) {
      body[f.field] = f.default;
    }
  }

  if (def.timestamps !== 'none') {
    body['created_at'] = '';
    if (def.timestamps !== 'created') {
      body['updated_at'] = '';
    }
  }
  return body;
}

async function confirm(question: string): Promise<boolean> {
  const rl = readline.createInterface({ input, output });
  try {
    const answer = (await rl.question(question)).trim().toLowerCase();
    return answer === 'y' || answer === 'yes';
  } finally {
    rl.close();
  }
}

function recordId(res: unknown, fallback: string): string {
  if (res && typeof res === 'object' && 'id' in res) {
    return String((res as Record<string, unknown>)['id']);
  }
  return fallback;
}

/**
 * Generic CRUD factory. One declarative {@link EntityDef} becomes a full
 * command group:
 *
 *   <entity> list [--json]          (default subcommand)
 *   <entity> get <id>
 *   <entity> create --id ... [fields]
 *   <entity> update <id> [fields]   (only when supportsUpdate is not false)
 *   <entity> delete <id> [-y]
 */
export function buildEntityCommand(def: EntityDef): Command {
  const group = new Command(def.command).description(
    `Manage ${def.labelPlural} (CRUD).`,
  );

  // ---- list (default) -----------------------------------------------------
  const listCmd = addGlobalOptions(
    group
      .command('list', { isDefault: true })
      .description(`List all ${def.labelPlural}.`),
  );
  listCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = await client.list(def.route);
      if (opts.json) {
        printJson(res);
        return;
      }
      const rows = Array.isArray(res)
        ? (res as Array<Record<string, unknown>>)
        : [];
      printTable(def.columns, rows);
    }),
  );

  // ---- get ----------------------------------------------------------------
  const getCmd = addGlobalOptions(
    group.command('get <id>').description(`Show a single ${def.label}.`),
  );
  getCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const id = String(args[0]);
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const res = await client.get(def.route, id);
      if (opts.json) {
        printJson(res);
        return;
      }
      if (res && typeof res === 'object') {
        printTable(def.columns, [res as Record<string, unknown>]);
      } else {
        console.log('(no record)');
      }
    }),
  );

  // ---- create -------------------------------------------------------------
  const createCmd = addGlobalOptions(
    group.command('create').description(`Create a ${def.label}.`),
  );
  attachFieldFlags(createCmd, def, true);
  createCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const client = new HydraClient(resolveConfig(opts));
      const body = buildBody(def, opts, true);
      const res = await client.create(def.route, body);
      if (opts.json) {
        printJson(res);
        return;
      }
      printSuccess(`${def.label} ${recordId(res, String(body['id']))} created`);
    }),
  );

  // ---- update -------------------------------------------------------------
  if (def.supportsUpdate !== false) {
    const updateCmd = addGlobalOptions(
      group.command('update <id>').description(`Update a ${def.label}.`),
    );
    attachFieldFlags(updateCmd, def, false);
    updateCmd.action(
      withErrorHandler(async (...args: unknown[]) => {
        const id = String(args[0]);
        const actionCmd = args[args.length - 1] as Command;
        const opts = effectiveOpts(actionCmd);
        const client = new HydraClient(resolveConfig(opts));
        const body = buildBody(def, opts, false);
        const res = await client.update(def.route, id, body);
        if (opts.json) {
          printJson(res);
          return;
        }
        printSuccess(`${def.label} ${id} updated`);
      }),
    );
  }

  // ---- delete -------------------------------------------------------------
  const deleteCmd = addGlobalOptions(
    group.command('delete <id>').description(`Delete a ${def.label}.`),
  );
  deleteCmd.option('-y, --yes', 'Skip the confirmation prompt.');
  deleteCmd.action(
    withErrorHandler(async (...args: unknown[]) => {
      const id = String(args[0]);
      const actionCmd = args[args.length - 1] as Command;
      const opts = effectiveOpts(actionCmd);
      const extra = actionCmd.opts() as { yes?: boolean };
      const client = new HydraClient(resolveConfig(opts));
      if (!extra.yes) {
        const ok = await confirm(`Delete ${def.label} ${id}? [y/N] `);
        if (!ok) {
          console.log('Aborted.');
          return;
        }
      }
      await client.delete(def.route, id);
      if (opts.json) {
        printJson({ deleted: id });
        return;
      }
      printSuccess(`${def.label} ${id} deleted`);
    }),
  );

  return group;
}

/** Attach commander options for every field; required fields become requiredOptions on create. */
function attachFieldFlags(cmd: Command, def: EntityDef, requireRequired: boolean): void {
  for (const f of def.fields) {
    addFieldFlag(cmd, f, requireRequired);
  }
}

function addFieldFlag(cmd: Command, f: FieldDef, requireRequired: boolean): void {
  if (f.kind === 'boolean') {
    cmd.option(f.flag, `${f.help} (sets ${f.field}=true)`);
    if (f.falseFlag) cmd.option(f.falseFlag, `Sets ${f.field}=false.`);
    return;
  }
  if (f.kind === 'number') {
    const parser = (v: string): number | null => parseNumber(v, f.nullable);
    if (requireRequired && f.required) {
      cmd.requiredOption(f.flag, f.help, parser);
    } else {
      cmd.option(f.flag, f.help, parser);
    }
    return;
  }
  // string
  if (requireRequired && f.required) {
    cmd.requiredOption(f.flag, f.help);
  } else {
    cmd.option(f.flag, f.help);
  }
}
