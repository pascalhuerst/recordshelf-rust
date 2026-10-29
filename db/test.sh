#!/bin/sh
# Applies setup.sql and all migrations to a throwaway Postgres cluster and
# runs the schema checks. Needs the Postgres server binaries (initdb, pg_ctl)
# and psql on the PATH. Nothing touches an existing database.
set -eu
cd "$(dirname "$0")"

dir=$(mktemp -d)
port=${PGTESTPORT:-54329}
trap 'pg_ctl -D "$dir/data" -m immediate stop >/dev/null 2>&1 || true; rm -rf "$dir"' EXIT

initdb -D "$dir/data" -U postgres --auth=trust >/dev/null
pg_ctl -D "$dir/data" -o "-p $port -k $dir -c listen_addresses=''" -l "$dir/log" -w start >/dev/null

psql_su() { psql -X -q -h "$dir" -p "$port" -U postgres "$@"; }
psql_su -v ON_ERROR_STOP=1 -v password="'test'" -f setup.sql >/dev/null

for f in migrations/*.sql; do
    echo "applying $f"
    psql -X -q -h "$dir" -p "$port" -U recordshelf -d recordshelf -v ON_ERROR_STOP=1 --single-transaction -f "$f"
done

psql -X -q -h "$dir" -p "$port" -U recordshelf -d recordshelf -v ON_ERROR_STOP=1 -f tests/schema.sql
