#!/usr/bin/env bash
#
# Stop the production stack. Data is preserved (no `-v`): the bind-mounted directories under
# prod/data are untouched.
set -euo pipefail

PROD="$(cd "$(dirname "$0")/.." && pwd)"

docker compose -f "$PROD/docker-compose.yml" --env-file "$PROD/.env" down --remove-orphans
