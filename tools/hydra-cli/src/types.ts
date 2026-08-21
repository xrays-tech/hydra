import type { ColDef } from './format.js';

export type FieldKind = 'string' | 'number' | 'boolean';

export interface FieldDef {
  /** Body field name as expected by the API. */
  field: string;
  kind: FieldKind;
  /** Commander flag spec, e.g. '--weight <n>' or '--enabled'. */
  flag: string;
  /** Help text. */
  help: string;
  /** Required for `create` (becomes a commander requiredOption). */
  required?: boolean;
  /** Number fields that may be cleared with the literal value "null". */
  nullable?: boolean;
  /** Default applied on create when the flag is absent. */
  default?: unknown;
  /** For boolean fields: the complementary flag that sets the value to false. */
  falseFlag?: string;
}

export interface EntityDef {
  /** Route segment under /api/v1, e.g. 'provider-models'. */
  route: string;
  /** CLI subcommand name, e.g. 'provider-models'. */
  command: string;
  /** Singular human label, e.g. 'provider model'. */
  label: string;
  /** Plural human label. */
  labelPlural: string;
  /** Columns rendered by the table formatter for list/get. */
  columns: ColDef[];
  /** Fields used to build create/update bodies. */
  fields: FieldDef[];
}

/**
 * Declarative description of every CRUD entity. The generic factory in
 * `commands/entities.ts` turns each entry into a full set of subcommands,
 * keeping the six entity groups DRY.
 */
export const ENTITY_DEFS: EntityDef[] = [
  {
    route: 'providers',
    command: 'providers',
    label: 'provider',
    labelPlural: 'providers',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'key', header: 'KEY', width: 18 },
      { field: 'name', header: 'NAME', width: 22 },
      { field: 'endpoint', header: 'ENDPOINT', width: 34 },
      { field: 'weight', header: 'WEIGHT', width: 7 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Provider id', required: true },
      { field: 'key', kind: 'string', flag: '--key <key>', help: 'Provider key', required: true },
      { field: 'name', kind: 'string', flag: '--name <name>', help: 'Display name', required: true },
      { field: 'endpoint', kind: 'string', flag: '--endpoint <url>', help: 'Upstream endpoint URL', required: true },
      { field: 'weight', kind: 'number', flag: '--weight <n>', help: 'Routing weight (integer)', required: true },
      { field: 'max_concurrency', kind: 'number', flag: '--max-concurrency <n>', help: 'Max concurrency (pass "null" to clear)', nullable: true },
      { field: 'max_queue_depth', kind: 'number', flag: '--max-queue-depth <n>', help: 'Max queue depth (pass "null" to clear)', nullable: true },
      { field: 'queue_wait_timeout_ms', kind: 'number', flag: '--queue-wait-timeout-ms <ms>', help: 'Queue wait timeout in ms (pass "null" to clear)', nullable: true },
    ],
  },
  {
    route: 'provider-models',
    command: 'provider-models',
    label: 'provider model',
    labelPlural: 'provider models',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'key', header: 'KEY', width: 22 },
      { field: 'name', header: 'NAME', width: 22 },
      { field: 'provider_id', header: 'PROVIDER', width: 18 },
      { field: 'status', header: 'STATUS', width: 7 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Model id', required: true },
      { field: 'key', kind: 'string', flag: '--key <key>', help: 'Model key', required: true },
      { field: 'name', kind: 'string', flag: '--name <name>', help: 'Display name', required: true },
      { field: 'provider_id', kind: 'string', flag: '--provider-id <id>', help: 'Owning provider id', required: true },
      { field: 'status', kind: 'number', flag: '--status <n>', help: 'Status (1 = active)', default: 1 },
    ],
  },
  {
    route: 'provider-keys',
    command: 'provider-keys',
    label: 'provider key',
    labelPlural: 'provider keys',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'provider_id', header: 'PROVIDER', width: 18 },
      { field: 'api_key', header: 'API_KEY (masked)', width: 42 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Key id', required: true },
      { field: 'provider_id', kind: 'string', flag: '--provider-id <id>', help: 'Owning provider id', required: true },
      { field: 'api_key', kind: 'string', flag: '--api-key <key>', help: 'Upstream API key value (stored masked server-side)', required: true },
    ],
  },
  {
    route: 'tenants',
    command: 'tenants',
    label: 'tenant',
    labelPlural: 'tenants',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'name', header: 'NAME', width: 22 },
      { field: 'domain', header: 'DOMAIN', width: 26 },
      { field: 'enabled', header: 'ENABLED', width: 8 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Tenant id', required: true },
      { field: 'name', kind: 'string', flag: '--name <name>', help: 'Display name', required: true },
      { field: 'domain', kind: 'string', flag: '--domain <domain>', help: 'Tenant domain', required: true },
      { field: 'auth_url', kind: 'string', flag: '--auth-url <url>', help: 'Auth URL', required: true },
      { field: 'enabled', kind: 'boolean', flag: '--enabled', falseFlag: '--disabled', help: 'Enable (--enabled) or disable (--disabled) the tenant', default: true },
      { field: 'cert_key', kind: 'string', flag: '--cert-key <key>', help: 'Cert key (nullable)' },
      { field: 'cert_file', kind: 'string', flag: '--cert-file <path>', help: 'Cert file path (nullable)' },
    ],
  },
  {
    route: 'tenant-providers',
    command: 'tenant-providers',
    label: 'tenant-provider mapping',
    labelPlural: 'tenant-provider mappings',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'tenant_id', header: 'TENANT', width: 18 },
      { field: 'provider_id', header: 'PROVIDER', width: 18 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Mapping id', required: true },
      { field: 'tenant_id', kind: 'string', flag: '--tenant-id <id>', help: 'Tenant id', required: true },
      { field: 'provider_id', kind: 'string', flag: '--provider-id <id>', help: 'Provider id', required: true },
    ],
  },
  {
    route: 'tenant-models',
    command: 'tenant-models',
    label: 'tenant-model mapping',
    labelPlural: 'tenant-model mappings',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'tenant_id', header: 'TENANT', width: 18 },
      { field: 'model_key', header: 'MODEL_KEY', width: 28 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Mapping id', required: true },
      { field: 'tenant_id', kind: 'string', flag: '--tenant-id <id>', help: 'Tenant id', required: true },
      { field: 'model_key', kind: 'string', flag: '--model-key <key>', help: 'Model key', required: true },
    ],
  },
  {
    route: 'provider-key-bindings',
    command: 'provider-key-bindings',
    label: 'key-prefix binding',
    labelPlural: 'key-prefix bindings',
    columns: [
      { field: 'id', header: 'ID', width: 18 },
      { field: 'key_prefix', header: 'PREFIX', width: 24 },
      { field: 'provider_id', header: 'PROVIDER', width: 18 },
      { field: 'enabled', header: 'ENABLED', width: 8 },
    ],
    fields: [
      { field: 'id', kind: 'string', flag: '--id <id>', help: 'Binding id', required: true },
      { field: 'key_prefix', kind: 'string', flag: '--key-prefix <prefix>', help: 'Client api-key prefix', required: true },
      { field: 'provider_id', kind: 'string', flag: '--provider-id <id>', help: 'Provider id', required: true },
      { field: 'enabled', kind: 'boolean', flag: '--enabled', falseFlag: '--disabled', help: 'Enable (--enabled) or disable (--disabled)', default: true },
    ],
  },
];
