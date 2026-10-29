#!/usr/bin/env bash
# Tooling for the RecordShelf database (PostgreSQL), built on psql.
#
#   db/db.sh bootstrap -h HOST -U ADMIN      create role and database, apply migrations
#   db/db.sh migrate   -h HOST -U USER       apply pending migrations
#   db/db.sh status    -h HOST -U USER       show which migrations are applied
#   db/db.sh drop      -h HOST -U ADMIN      delete the database and its role (asks first)
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
# drop needs a superuser as well. It shows what will be deleted and asks you
# to type the database name; --confirm NAME does that without a prompt (for
# scripts). Everything in the database is lost. The role is dropped too unless
# --keep-role is given or it owns other databases.
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
    bootstrap|migrate|status|drop) ;;
    -h|--help|help) usage 0 ;;
    *) echo "unknown command: $COMMAND" >&2; usage 1 ;;
esac

HOST=${PGHOST:-}
PORT=${PGPORT:-}
USER_NAME=${PGUSER:-}
DB_NAME=recordshelf
ROLE=recordshelf
CONFIRM=
KEEP_ROLE=

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--host) HOST=${2:?missing host}; shift 2 ;;
        -p|--port) PORT=${2:?missing port}; shift 2 ;;
        -U|--user) USER_NAME=${2:?missing user}; shift 2 ;;
        -d|--dbname) DB_NAME=${2:?missing database name}; shift 2 ;;
        -r|--role) ROLE=${2:?missing role name}; shift 2 ;;
        --confirm) CONFIRM=${2:?missing database name}; shift 2 ;;
        --keep-role) KEEP_ROLE=1; shift ;;
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

cmd_drop() {
    case "$DB_NAME" in
        postgres|template0|template1) die "refusing to drop the system database $DB_NAME" ;;
    esac
    check_connection postgres
    local superuser
    superuser=$(query postgres -c "select rolsuper from pg_roles where rolname = current_user")
    [ "$superuser" = "t" ] || die "drop needs a superuser (-U postgres); ${USER_NAME:-the current user} is not one"

    local db_exists role_exists
    db_exists=$(query postgres -c "select 1 from pg_database where datname = '$DB_NAME'")
    role_exists=$(query postgres -c "select 1 from pg_roles where rolname = '$ROLE'")
    if [ -z "$db_exists" ] && [ -z "$role_exists" ]; then
        echo "database $DB_NAME and role $ROLE do not exist; nothing to do"
        return 0
    fi

    # The role is only dropped if it owns nothing but this database.
    local drop_role=
    if [ -n "$role_exists" ] && [ -z "$KEEP_ROLE" ]; then
        local others
        others=$(query postgres -c "select string_agg(datname, ', ') from pg_database where datdba = (select oid from pg_roles where rolname = '$ROLE') and datname <> '$DB_NAME'")
        if [ -n "$others" ]; then
            echo "note: role $ROLE also owns the database(s) $others and will be kept"
        else
            drop_role=1
        fi
    fi

    echo "this will permanently delete:"
    if [ -n "$db_exists" ]; then
        local info
        info=$(query postgres -c "select pg_size_pretty(pg_database_size('$DB_NAME'))")
        local tables=
        tables=$(query "$DB_NAME" -c "select count(*) from pg_tables where schemaname = 'public'" 2>/dev/null || true)
        echo "  database $DB_NAME ($info${tables:+, $tables tables}) on ${HOST:-the local server}${PORT:+:$PORT}"
    else
        echo "  (database $DB_NAME does not exist)"
    fi
    [ -n "$drop_role" ] && echo "  role $ROLE"

    local answer
    if [ -n "$CONFIRM" ]; then
        answer=$CONFIRM
    elif [ -t 0 ]; then
        read -r -p "type the database name ($DB_NAME) to confirm: " answer
    else
        die "not confirmed: run it in a terminal, or pass --confirm $DB_NAME"
    fi
    [ "$answer" = "$DB_NAME" ] || die "not confirmed; nothing was deleted"

    if [ -n "$db_exists" ]; then
        # disconnect everyone first (works on every server version)
        query postgres -c "select pg_terminate_backend(pid) from pg_stat_activity where datname = '$DB_NAME' and pid <> pg_backend_pid()" >/dev/null
        run_psql postgres -c "DROP DATABASE \"$DB_NAME\""
        echo "dropped database $DB_NAME"
    fi
    if [ -n "$drop_role" ]; then
        run_psql postgres -c "DROP ROLE \"$ROLE\""
        echo "dropped role $ROLE"
    elif [ -n "$role_exists" ]; then
        echo "kept role $ROLE"
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
    drop) cmd_drop ;;
esac
