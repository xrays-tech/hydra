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

/**
 * Carrier for "clear this field" (JSON `null`) through commander's option value.
 *
 * A custom parser that returns `null` does NOT survive: commander 12.1.0 stores `''`
 * instead (measured with a two-line probe). That is what made the documented
 * `providers update <id> --max-concurrency null` send `"max_concurrency": ""`, which
 * the server rejects with `400 invalid_json: invalid type: string "", expected u32` —
 * the "clear" did nothing but print a confusing error.
 *
 * A non-numeric string cannot collide with a real value: every other non-number is
 * rejected by `InvalidArgumentError` in `parseNumber`.
 */
export const CLEAR_VALUE = 'null';

function parseNumber(value: string, nullable?: boolean): number | string {
  if (nullable && value.toLowerCase() === CLEAR_VALUE) return CLEAR_VALUE;
  const n = Number(value);
  if (!Number.isFinite(n)) {
    throw new InvalidArgumentError(`expected a number, got "${value}"`);
  }
  return n;
}

/** The JSON value one parsed option contributes to a request body. */
function fieldValue(f: FieldDef, value: unknown): unknown {
  if (f.kind === 'number' && f.nullable && value === CLEAR_VALUE) return null;
  return value;
}

/**
 * Build the JSON body for create/update from parsed options.
 *
 * Some server entities auto-fill created_at/updated_at when blank; for those
 * we send "" so callers never have to. Entities without timestamps (or with
 * only created_at) are handled by `def.timestamps`.
 *
 * Exported for the test suite: `--max-concurrency null` never reaching the body as
 * a JSON `null` is exactly the bug that had no test.
 *
 * @param isCreate when true, defaults are applied for omitted fields.
 */
export function buildBody(
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
      body[f.field] = fieldValue(f, opts[key]);
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

/** Fields whose read-back from the server is NOT the stored value, so they must
 *  never be merged back into an update body. `provider-keys` reads are masked
 *  (`first10…last4`), and writing the mask over the key would destroy it. */
const READBACK_UNSAFE: Record<string, string[]> = {
  'provider-keys': ['api_key'],
};

/** Merge a PARTIAL update onto the record read back from the server.
 *
 *  The admin API replaces the whole record (`UPDATE provider SET <every column>`,
 *  and the handler deserialises the body straight into the entity), so sending only
 *  the flags the user typed fails with `400 invalid_json: missing field …` —
 *  measured: `providers update p --weight 5` -> `missing field \`id\``. This was
 *  true for EVERY group with an `update` subcommand.
 *
 *  Exported for the test suite: its absence was the bug. */
export function mergeForUpdate(
  def: EntityDef,
  current: unknown,
  partial: Record<string, unknown>,
): Record<string, unknown> {
  const base = (current && typeof current === 'object' ? { ...(current as Record<string, unknown>) } : {});
  for (const f of READBACK_UNSAFE[def.route] ?? []) delete base[f];
  const merged: Record<string, unknown> = { ...base, ...partial };
  // Response-only decorations must not travel back (serde ignores unknown fields
  // today, but relying on that is how a contract drifts).
  delete merged['snapshot_stale'];
  delete merged['has_access_token'];
  return merged;
}

/** Fields a merge CANNOT supply: the server masks them on read, so the
 *  operator has to provide them explicitly. Returns the missing flag names. */
export function unmergeableMissing(def: EntityDef, merged: Record<string, unknown>): string[] {
  return (READBACK_UNSAFE[def.route] ?? [])
    .filter((f) => merged[f] === undefined)
    .map((f) => '--' + f.replace(/_/g, '-'));
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
        const partial = buildBody(def, opts, false);
        // Read-modify-write: see `mergeForUpdate` for why a partial body cannot work.
        const current = await client.get(def.route, id);
        const body = mergeForUpdate(def, current, partial);
        const missing = unmergeableMissing(def, body);
        if (missing.length > 0) {
          throw new Error(
            `${def.label} update needs ${missing.join(', ')}: the server masks that ` +
              'value on read, so it cannot be carried over from the current record.',
          );
        }
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
    // `number | string`: a nullable field's "clear" comes back as `CLEAR_VALUE`
    // (commander drops a `null` returned by a parser).
    const parser = (v: string): number | string => parseNumber(v, f.nullable);
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
