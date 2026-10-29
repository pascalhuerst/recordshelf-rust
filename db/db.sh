#!/usr/bin/env bash
# Tooling for the RecordShelf database (PostgreSQL), built on psql.
#
#   db/db.sh bootstrap -h HOST -U ADMIN      create role and database, apply migrations
#   db/db.sh migrate   -h HOST -U USER       apply pending migrations
#   db/db.sh status    -h HOST -U USER       show which migrations are applied
#
# Connection options (like psql): -h host, -p port, -U user, -d database
# (default recordshelf). Passwords come from the usual places: PGPASSWORD,
# ~/.pgpass, or trust authentication on the server.
#
# bootstrap needs a superuser (e.g. -U postgres). It is safe to run again: it
# only creates what is missing and applies migrations that are still pending.
# The role is created with a generated password unless RECORDSHELF_DB_PASSWORD
# is set; a generated password is printed once, at the end.
#
# Migrations (db/migrations/NNNN_name.sql) are applied in order, each in its
# own transaction together with its entry in schema_migrations. A migration
# that was changed after it was applied is refused (checksum).

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
MIGRATIONS_DIR=${RECORDSHELF_MIGRATIONS_DIR:-$SCRIPT_DIR/migrations}

die() { echo "error: $*" >&2; exit 1; }

usage() {
    sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' | head -n -1
    exit "${1:-0}"
}

command -v psql >/dev/null || die "psql not found; install the PostgreSQL client"
command -v sha256sum >/dev/null || die "sha256sum not found"

[ $# -ge 1 ] || usage 1
COMMAND=$1
shift
case "$COMMAND" in
    bootstrap|migrate|status) ;;
    -h|--help|help) usage 0 ;;
    *) echo "unknown command: $COMMAND" >&2; usage 1 ;;
esac

HOST=${PGHOST:-}
PORT=${PGPORT:-}
USER_NAME=${PGUSER:-}
DB_NAME=recordshelf
ROLE=recordshelf
SET_ROLE=

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--host) HOST=${2:?missing host}; shift 2 ;;
        -p|--port) PORT=${2:?missing port}; shift 2 ;;
        -U|--user) USER_NAME=${2:?missing user}; shift 2 ;;
        -d|--dbname) DB_NAME=${2:?missing database name}; shift 2 ;;
        -r|--role) ROLE=${2:?missing role name}; shift 2 ;;
        --help) usage 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

# Names end up in SQL, so keep them boring.
name_ok() { [[ $1 =~ ^[a-z_][a-z0-9_]{0,62}$ ]]; }
name_ok "$DB_NAME" || die "invalid database name: $DB_NAME"
name_ok "$ROLE" || die "invalid role name: $ROLE"

# psql with the connection options. $1 is the database to connect to.
run_psql() {
    local db=$1
    shift
    local args=(-X -q -v ON_ERROR_STOP=1 -w)
    [ -n "$HOST" ] && args+=(-h "$HOST")
    [ -n "$PORT" ] && args+=(-p "$PORT")
    [ -n "$USER_NAME" ] && args+=(-U "$USER_NAME")
    psql "${args[@]}" -d "$db" "$@"
}

# Queries returning plain values, one row per line, fields separated by '|'.
query() {
    local db=$1
    shift
    run_psql "$db" -At -F '|' "$@"
}

check_connection() {
    local db=$1
    query "$db" -c "select 1" >/dev/null 2>"${TMPDIR:-/tmp}/rsdb.$$" || {
        echo "error: cannot connect to ${HOST:-the local server}${PORT:+:$PORT} as ${USER_NAME:-the default user} (database $db):" >&2
        sed 's/^/  /' "${TMPDIR:-/tmp}/rsdb.$$" >&2
        rm -f "${TMPDIR:-/tmp}/rsdb.$$"
        exit 1
    }
    rm -f "${TMPDIR:-/tmp}/rsdb.$$"
}

# --- migrations ----------------------------------------------------------

# Prints "version|name|path|checksum" for every migration file, in order.
list_migrations() {
    local file name version previous=-1
    shopt -s nullglob
    for file in "$MIGRATIONS_DIR"/*.sql; do
        name=$(basename "$file")
        [[ $name =~ ^([0-9]{4})_[a-z0-9_]+\.sql$ ]] || die "bad migration file name: $name (expected NNNN_description.sql)"
        version=$((10#${BASH_REMATCH[1]}))
        [ "$version" -gt "$previous" ] || die "migration number $version is used twice: $name"
        previous=$version
        echo "$version|$name|$file|$(sha256sum "$file" | cut -d' ' -f1)"
    done
}

ensure_migrations_table() {
    run_psql "$DB_NAME" <<'SQL'
CREATE TABLE IF NOT EXISTS schema_migrations (
    version    integer PRIMARY KEY,
    name       text NOT NULL,
    checksum   text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
);
SQL
}

# Applies pending migrations (or only reports with dry=1). Stops at the first
# problem. Prints one line per migration.
migrate() {
    local dry=${1:-0}
    local applied
    if [ "$(query "$DB_NAME" -c "select to_regclass('schema_migrations') is not null")" = "t" ]; then
        applied=$(query "$DB_NAME" -c "select version, checksum from schema_migrations order by version")
    else
        [ "$dry" = 1 ] || ensure_migrations_table
        applied=
    fi

    local version name file checksum known pending=0 done_count=0
    while IFS='|' read -r version name file checksum; do
        [ -n "$version" ] || continue
        known=$(printf '%s\n' "$applied" | awk -F'|' -v v="$version" '$1 == v { print $2 }')
        if [ -n "$known" ]; then
            if [ "$known" != "$checksum" ]; then
                die "migration $name was changed after it was applied (checksum differs). Never edit an applied migration; add a new one."
            fi
            [ "$dry" = 1 ] && echo "applied  $name"
            continue
        fi
        pending=$((pending + 1))
        if [ "$dry" = 1 ]; then
            echo "pending  $name"
            continue
        fi
        echo "applying $name"
        # the migration and its bookkeeping row commit together or not at all
        run_psql "$DB_NAME" --single-transaction -f "$file" \
            -c "INSERT INTO schema_migrations (version, name, checksum) VALUES ($version, '$name', '$checksum')" \
            || die "migration $name failed; nothing of it was applied"
        done_count=$((done_count + 1))
    done < <(list_migrations)

    if [ "$dry" != 1 ]; then
        if [ "$done_count" -eq 0 ]; then echo "database is up to date"; else echo "applied $done_count migration(s)"; fi
    fi
}

# --- commands ------------------------------------------------------------

cmd_status() {
    check_connection "$DB_NAME"
    [ -n "$(list_migrations)" ] || die "no migrations found in $MIGRATIONS_DIR"
    migrate 1
}

cmd_migrate() {
    check_connection "$DB_NAME"
    [ -n "$(list_migrations)" ] || die "no migrations found in $MIGRATIONS_DIR"
    migrate 0
}

cmd_bootstrap() {
    [ -n "$(list_migrations)" ] || die "no migrations found in $MIGRATIONS_DIR"
    check_connection postgres
    local superuser
    superuser=$(query postgres -c "select rolsuper from pg_roles where rolname = current_user")
    [ "$superuser" = "t" ] || die "bootstrap needs a superuser (-U postgres); ${USER_NAME:-the current user} is not one"

    local generated=
    local password=${RECORDSHELF_DB_PASSWORD:-}
    local role_exists
    role_exists=$(query postgres -c "select 1 from pg_roles where rolname = '$ROLE'")
    if [ -z "$role_exists" ]; then
        if [ -z "$password" ]; then
            password=$(head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n')
            generated=1
        fi
        # the password goes through the environment, not the command line
        RS_ROLE_PASSWORD=$password run_psql postgres -v role="$ROLE" <<'SQL'
\getenv pw RS_ROLE_PASSWORD
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', :'role', :'pw') \gexec
SQL
        echo "created role $ROLE"
    else
        echo "role $ROLE exists (password left unchanged)"
    fi

    if [ -z "$(query postgres -c "select 1 from pg_database where datname = '$DB_NAME'")" ]; then
        run_psql postgres -v role="$ROLE" -v db="$DB_NAME" <<'SQL'
SELECT format('CREATE DATABASE %I OWNER %I ENCODING ''UTF8''', :'db', :'role') \gexec
SQL
        echo "created database $DB_NAME"
    else
        echo "database $DB_NAME exists"
    fi

    # Connected as the superuser, but acting as the role, so that everything
    # created by the migrations belongs to it.
    PGOPTIONS="-c role=$ROLE ${PGOPTIONS:-}" migrate_as_role

    echo
    echo "connection string for the settings file (database_url):"
    if [ -n "$generated" ]; then
        echo "  postgres://$ROLE:$password@${HOST:-localhost}${PORT:+:$PORT}/$DB_NAME"
        echo "the password was generated just now and is not stored anywhere else; keep it."
    else
        echo "  postgres://$ROLE:<password>@${HOST:-localhost}${PORT:+:$PORT}/$DB_NAME"
    fi
}

migrate_as_role() {
    export PGOPTIONS
    check_connection "$DB_NAME"
    migrate 0
}

case "$COMMAND" in
    bootstrap) cmd_bootstrap ;;
    migrate) cmd_migrate ;;
    status) cmd_status ;;
esac
