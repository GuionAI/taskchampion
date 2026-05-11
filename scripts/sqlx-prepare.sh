#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PG_URL="${SQLX_POSTGRES_DATABASE_URL:-postgres://supabase_admin:dev-password@localhost:30432/supabase?sslmode=disable&search_path=public}"

cd "$ROOT"
rm -rf .sqlx

cargo sqlx prepare -D "$PG_URL" -- \
  -p taskchampion \
  --no-default-features \
  --features storage-pgwire \
  --all-targets
