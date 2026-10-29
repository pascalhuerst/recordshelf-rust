#!/bin/sh
# Runs the database tests on a throwaway Postgres cluster in a temp directory:
# the tooling (db.sh bootstrap/migrate/status) and the schema rules. Needs the
# Postgres server binaries (initdb, pg_ctl) and psql on the PATH. Nothing
# touches an existing database.
set -eu
cd "$(dirname "$0")"

dir=$(mktemp -d)
export PGPORT=${PGTESTPORT:-54329}
export PGHOST=$dir
trap 'pg_ctl -D "$dir/data" -m immediate stop >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

initdb -D "$dir/data" -U postgres --auth=trust >/dev/null
pg_ctl -D "$dir/data" -o "-p $PGPORT -k $dir -c listen_addresses=''" -l "$dir/log" -w start >/dev/null

bash tests/bootstrap.sh

# the schema rules, on a database made by the tooling
./db.sh bootstrap -U postgres -d schema_checks >/dev/null
psql -X -q -U recordshelf -d schema_checks -v ON_ERROR_STOP=1 -f tests/schema.sql
