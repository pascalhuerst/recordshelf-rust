#!/usr/bin/env bash
# Tests of db.sh against a throwaway cluster (see test.sh, which sets
# PGHOST/PGPORT and trust authentication).
set -uo pipefail
cd "$(dirname "$0")/.."

DB=./db.sh
failures=0
checks=0

ok() { checks=$((checks + 1)); }
fail() { failures=$((failures + 1)); checks=$((checks + 1)); echo "FAIL: $*" >&2; }

expect_in() { # expect_in "text" "$output" "what"
    if grep -qF -- "$1" <<<"$2"; then ok; else fail "$3: expected '$1' in:"$'\n'"$2"; fi
}
expect_not_in() {
    if grep -qF -- "$1" <<<"$2"; then fail "$3: did not expect '$1' in:"$'\n'"$2"; else ok; fi
}
expect_eq() { # expect_eq expected actual what
    if [ "$1" = "$2" ]; then ok; else fail "$3: expected '$1', got '$2'"; fi
}
sql() { psql -X -At -h "$PGHOST" -p "$PGPORT" -U postgres -d "$1" -c "$2"; }

# --- fresh bootstrap ---------------------------------------------------------
out=$($DB bootstrap -U postgres -d t1 -r t1_role 2>&1); code=$?
expect_eq 0 "$code" "fresh bootstrap exit code"
expect_in "created role t1_role" "$out" "role created"
expect_in "created database t1" "$out" "database created"
expect_in "applying 0001_init.sql" "$out" "migration applied"
expect_in "applied 1 migration(s)" "$out" "summary"
expect_in "postgres://t1_role:" "$out" "connection string"
password=$(grep -o 'postgres://t1_role:[0-9a-f]*@' <<<"$out" | sed 's|postgres://t1_role:||; s|@||')
expect_eq 36 "${#password}" "generated password length"
expect_eq "t|t" "$(sql postgres "select rolcanlogin, rolpassword is not null from pg_roles where rolname = 't1_role'")" "role can log in and has a password"
expect_eq "t1_role" "$(sql postgres "select pg_get_userbyid(datdba) from pg_database where datname = 't1'")" "database owner"
expect_eq "t1_role" "$(sql t1 "select tableowner from pg_tables where tablename = 'releases'")" "tables are owned by the role"
expect_eq "t1_role" "$(sql t1 "select tableowner from pg_tables where tablename = 'schema_migrations'")" "bookkeeping table owner"
expect_eq "1|0001_init.sql" "$(sql t1 "select version, name from schema_migrations")" "recorded migration"
expect_eq 64 "$(sql t1 "select length(checksum) from schema_migrations")" "checksum recorded"
expect_eq "t" "$(sql t1 "select count(*) > 10 from pg_tables where schemaname = 'public'")" "tables created"

# --- running it again changes nothing ------------------------------------------
out=$($DB bootstrap -U postgres -d t1 -r t1_role 2>&1); code=$?
expect_eq 0 "$code" "second bootstrap exit code"
expect_in "role t1_role exists (password left unchanged)" "$out" "role kept"
expect_in "database t1 exists" "$out" "database kept"
expect_in "database is up to date" "$out" "nothing to apply"
expect_in "postgres://t1_role:<password>@" "$out" "no password printed again"
expect_not_in "$password" "$out" "the old password must not reappear"
expect_eq 1 "$(sql t1 "select count(*) from schema_migrations")" "no duplicate bookkeeping rows"

# --- status ------------------------------------------------------------------------
out=$($DB status -U postgres -d t1 2>&1)
expect_in "applied  0001_init.sql" "$out" "status of an applied migration"

# --- a new migration is applied on its own ---------------------------------------
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp migrations/*.sql "$tmp"/
echo "ALTER TABLE releases ADD COLUMN test_col text;" >"$tmp/0002_add_test_col.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB status -U postgres -d t1 2>&1)
expect_in "pending  0002_add_test_col.sql" "$out" "status of a pending migration"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 0 "$code" "migrate exit code"
expect_in "applying 0002_add_test_col.sql" "$out" "new migration applied"
expect_not_in "applying 0001" "$out" "old migration not repeated"
expect_eq "test_col" "$(sql t1 "select column_name from information_schema.columns where table_name = 'releases' and column_name = 'test_col'")" "column added"
expect_eq 2 "$(sql t1 "select count(*) from schema_migrations")" "bookkeeping rows"

# --- an edited migration is refused ----------------------------------------------
echo "-- edited later" >>"$tmp/0001_init.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 1 "$code" "edited migration exit code"
expect_in "was changed after it was applied" "$out" "edited migration message"
cp migrations/0001_init.sql "$tmp/0001_init.sql"

# --- a failing migration leaves nothing behind -----------------------------------
printf 'CREATE TABLE half_done (id integer);\nSELECT this_is_not_valid;\n' >"$tmp/0003_broken.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 1 "$code" "failing migration exit code"
expect_in "migration 0003_broken.sql failed" "$out" "failing migration message"
expect_eq "" "$(sql t1 "select to_regclass('half_done')")" "partial changes are rolled back"
expect_eq 2 "$(sql t1 "select count(*) from schema_migrations")" "failed migration is not recorded"
printf 'CREATE TABLE half_done (id integer);\n' >"$tmp/0003_broken.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 0 "$code" "fixed migration exit code"
expect_in "applying 0003_broken.sql" "$out" "fixed migration is applied"

# --- password from the environment is used and never echoed ----------------------
out=$(RECORDSHELF_DB_PASSWORD=sup3rsecret $DB bootstrap -U postgres -d t2 -r t2_role 2>&1); code=$?
expect_eq 0 "$code" "bootstrap with a given password"
expect_not_in "sup3rsecret" "$out" "a given password is not echoed"
expect_in "postgres://t2_role:<password>@" "$out" "placeholder in the connection string"
expect_eq "t" "$(sql postgres "select rolpassword is not null from pg_roles where rolname = 't2_role'")" "given password was set"

# --- bad input is rejected before anything is touched ------------------------------
out=$($DB bootstrap -U postgres -d 'bad;name' 2>&1); code=$?
expect_eq 1 "$code" "bad database name exit code"
expect_in "invalid database name" "$out" "bad database name"
out=$($DB bootstrap -U postgres -r 'Bad-Role' 2>&1); code=$?
expect_in "invalid role name" "$out" "bad role name"
out=$($DB bootstrap -U t1_role -d t3 -r t3_role 2>&1); code=$?
expect_eq 1 "$code" "non-superuser exit code"
expect_in "needs a superuser" "$out" "non-superuser message"
expect_eq "" "$(sql postgres "select 1 from pg_roles where rolname = 't3_role'")" "nothing created by a non-superuser"
out=$(PGPORT=1 $DB bootstrap -U postgres 2>&1); code=$?
expect_eq 1 "$code" "unreachable server exit code"
expect_in "cannot connect" "$out" "unreachable server message"
out=$($DB frobnicate 2>&1); code=$?
expect_eq 1 "$code" "unknown command exit code"
expect_in "unknown command" "$out" "unknown command message"

empty=$(mktemp -d)
out=$(RECORDSHELF_MIGRATIONS_DIR=$empty $DB migrate -U postgres -d t1 2>&1); code=$?
expect_in "no migrations found" "$out" "empty migrations directory"
echo "SELECT 1;" >"$empty/001_short_number.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$empty $DB migrate -U postgres -d t1 2>&1)
expect_in "bad migration file name" "$out" "badly named migration"
rm -f "$empty"/*.sql
echo "SELECT 1;" >"$empty/0001_a.sql"; echo "SELECT 2;" >"$empty/0001_b.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$empty $DB migrate -U postgres -d t1 2>&1)
expect_in "used twice" "$out" "duplicate migration number"
rm -rf "$empty"

out=$($DB --help 2>&1)
expect_in "db/db.sh bootstrap" "$out" "help text"

echo "bootstrap tests: $((checks - failures))/$checks passed"
[ "$failures" -eq 0 ]
