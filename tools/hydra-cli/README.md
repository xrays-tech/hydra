# hydra-admin

A command-line client for the **Hydra** LLM gateway admin REST API.

`hydra-admin` drives every admin endpoint — health, hot-reload, metrics, live
concurrency, circuit breakers, auth-cache invalidation, usage stats, cluster
status, tenant auth-url probing, and full CRUD for providers, provider-models,
provider-keys, tenants, tenant-providers, tenant-models, limit-roles and
key-prefix bindings — straight from the terminal.

The npm package and the command you run are both called **`hydra-admin`**:
`npm i -g hydra-admin`, then `hydra-admin ...`.

- Runtime: Node.js >= 18 (uses the built-in `fetch`).
- Single runtime dependency: [`commander`](https://www.npmjs.com/package/commander).
- Ships a single bundled, shebanged file.

---

## Install

```bash
# from npm
npm install -g hydra-admin
hydra-admin --version

# or run once without installing
npx hydra-admin --help
```

### Build from source

```bash
cd tools/hydra-cli
npm install
npm run build       # produces dist/cli.js
./dist/cli.js --help
# or link it globally:
npm link
hydra-admin --help
```

---

## Configuration

Every command needs the Hydra **base URL** and an **admin bearer token**. Provide
them via flags or environment variables (flags win).

| Flag          | Environment            | Default                  |
| ------------- | ---------------------- | ------------------------ |
| `--base-url`  | `HYDRA_BASE_URL` / `HYDRA_HOST` | `http://127.0.0.1:8081` |
| `--token`     | `HYDRA_ADMIN_TOKEN`    | _required_               |

Extras:

| Flag            | Purpose                                          |
| --------------- | ------------------------------------------------ |
| `--json`        | Print the raw JSON response, skip table output.  |
| `-v, --verbose` | Print the HTTP method + URL to stderr.           |

```bash
export HYDRA_ADMIN_TOKEN="s3cret"
export HYDRA_BASE_URL="http://127.0.0.1:8081"

hydra-admin health
hydra-admin providers list --json
```

Global options may appear **before or after** the subcommand:

```bash
hydra-admin --token s3cret providers list
hydra-admin providers list --token s3cret
```

---

## Command reference

### Service endpoints

```bash
hydra-admin health                # service health
hydra-admin reload                # hot-reload config snapshot
hydra-admin metrics               # Prometheus text (raw, pipe to a file / scraper)
hydra-admin concurrency           # live admission / gate state
hydra-admin breaker               # list circuit-breaker dead providers
hydra-admin breaker reset <id>    # force-clear a provider's breaker
hydra-admin auth-cache invalidate # invalidate cached auth decisions
hydra-admin stats usage           # aggregated usage stats by tenant/provider
hydra-admin cluster status        # whole-cluster fleet status
hydra-admin tenants auth-test <auth-url> [--tenant-id <id>]  # probe a tenant auth URL
```

### Entity CRUD

All entity groups share the same shape:

```
hydra-admin <entity> list [--json]        # list all (default subcommand)
hydra-admin <entity> get <id>
hydra-admin <entity> create --id <id> [fields...]
hydra-admin <entity> update <id> [fields...]
hydra-admin <entity> delete <id> [-y]
```

Entities: `providers`, `provider-models`, `provider-keys`, `tenants`,
`tenant-providers`, `tenant-models`, `limit-roles`, `provider-key-bindings`.

Most entities support `update`; the mapping-only entities
(`tenant-providers` and `tenant-models`) intentionally omit it because the
server treats them as create/delete grants.

`delete` prompts for confirmation unless you pass `-y` / `--yes`.

#### providers

```bash
hydra-admin providers create \
  --id openai --key openai --name OpenAI \
  --endpoint https://api.openai.com --weight 10 \
  --max-concurrency 100 --max-queue-depth 50 --queue-wait-timeout-ms 5000

hydra-admin providers update openai --weight 20
hydra-admin providers update openai --max-concurrency null   # clear (set to null)
hydra-admin providers delete openai -y
```

| Field                   | Flag                            | Type          |
| ----------------------- | ------------------------------- | ------------- |
| `id`                    | `--id`                          | string        |
| `key`                   | `--key`                         | string        |
| `name`                  | `--name`                        | string        |
| `endpoint`              | `--endpoint`                    | string        |
| `weight`                | `--weight`                      | number        |
| `max_concurrency`       | `--max-concurrency`             | number / null |
| `max_queue_depth`       | `--max-queue-depth`             | number / null |
| `queue_wait_timeout_ms` | `--queue-wait-timeout-ms`       | number / null |

> Nullable numeric fields can be cleared by passing the literal value `null`.

#### provider-models

```bash
hydra-admin provider-models create \
  --id gpt-4o --key gpt-4o --name "GPT-4o" --provider-id openai --status 1
```

#### provider-keys

The upstream `api_key` is always stored and returned **masked** server-side.

```bash
hydra-admin provider-keys create --id openai-key --provider-id openai --api-key sk-xxxx
hydra-admin provider-keys list
```

#### tenants

`--enabled` / `--disabled` toggle the boolean `enabled` field (defaults to
enabled on create).

```bash
hydra-admin tenants create \
  --id acme --name Acme --domain acme.example.com \
  --auth-url https://auth.acme.example.com --enabled

hydra-admin tenants update acme --disabled --cert-key ./certs/acme.key
```

| Field            | Flag                  | Type    |
| ---------------- | --------------------- | ------- |
| `id`             | `--id`                | string  |
| `name`           | `--name`              | string  |
| `domain`         | `--domain`            | string  |
| `auth_url`       | `--auth-url`          | string  |
| `enabled`        | `--enabled` / `--disabled` | boolean |
| `cert_key`       | `--cert-key`          | string  |
| `cert_file`      | `--cert-file`         | string  |
| `cert_pem`       | `--cert-pem`          | string  |
| `cert_key_pem`   | `--cert-key-pem`      | string  |
| `access_token`   | `--access-token`      | string  |

`access_token` is write-only: it is stored as a SHA-256 hash, never echoed
back. Blank on edit keeps the current token; `--access-token ""` clears it.

#### tenant-providers / tenant-models

These are grant mappings; the server does not implement a PUT update for them.
Use delete + create to change a mapping.

```bash
hydra-admin tenant-providers create --id tp1 --tenant-id acme --provider-id openai
hydra-admin tenant-models   create --id tm1 --tenant-id acme --model-key gpt-4o
hydra-admin tenant-providers delete tp1 -y
```

#### limit-roles

Rate-limit roles matched by tenant / key / model / provider.

```bash
hydra-admin limit-roles create \
  --id rl1 --name basic --matching-tenant acme \
  --limit-count 100 --limit-token 100000 --window h --enabled

hydra-admin limit-roles update rl1 --limit-count 50 --disabled
hydra-admin limit-roles list
```

#### provider-key-bindings

Client api-keys whose raw value starts with `key_prefix` are pinned to one
provider.

```bash
hydra-admin provider-key-bindings create --id b1 --key-prefix sk_aaa_ --provider-id openai
hydra-admin provider-key-bindings update b1 --provider-id anthropic --disabled
```

---

## Output

- **Default (human-readable):** `list`/`get` render a compact column table;
  `create`/`update`/`delete` print a one-liner such as `✓ provider openai created`.
- **`--json`:** the raw JSON response, pretty-printed with 2-space indent.
- **`metrics`:** raw Prometheus text, verbatim.
- **Errors:** `Error: <message>` (including HTTP status and a body excerpt) on
  stderr, exit code `1`.

Set `NO_COLOR` to disable ANSI styling.

---

## Examples

```bash
# Pipe metrics to a file
hydra-admin metrics > hydra.prom

# Pretty-print all providers as JSON
hydra-admin providers list --json | jq .

# See exactly what's being sent
hydra-admin -v providers get openai
```

---

## Development

```bash
npm run build      # tsup -> dist/cli.js
npm run typecheck  # tsc --noEmit
npm test           # tsc (tests) + node --test against dist-test/test/client.test.js
npm pack --dry-run # inspect the published tarball
```

## License

MIT © 2026 ipconfiger
