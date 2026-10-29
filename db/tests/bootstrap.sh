#!/usr/bin/env bash
# Tests of db.sh against a throwaway cluster (see test.sh, which sets
# PGHOST/PGPORT and trust authentication).
set -uo pipefail
cd "$(dirname "$0")/.."

DB=./db.sh
# the real migrations; the tests add their own after these
N=$(ls migrations/*.sql | wc -l)
NEXT=$(printf '%04d' $((N + 1)))
NEXT2=$(printf '%04d' $((N + 2)))
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
expect_in "applied $N migration(s)" "$out" "summary"
expect_in "postgres://t1_role:" "$out" "connection string"
SETTINGS="$XDG_CONFIG_HOME/recordshelf/config.toml"
expect_in "wrote database_url to $SETTINGS" "$out" "settings file written"
expect_in "postgres://t1_role:<password>@" "$out" "masked connection string"
password=$(sed -n 's|^database_url = "postgres://t1_role:\([0-9a-f]*\)@.*|\1|p' "$SETTINGS")
expect_eq 36 "${#password}" "generated password length (read back from the settings file)"
expect_not_in "$password" "$out" "the password is not printed when it is stored"
expect_eq "600" "$(stat -c %a "$SETTINGS")" "settings file is private"
expect_eq 1 "$(grep -c '^database_url' "$SETTINGS")" "exactly one database_url"
expect_in "/t1\"" "$(grep '^database_url' "$SETTINGS")" "url points at the database"
expect_eq "t|t" "$(sql postgres "select rolcanlogin, rolpassword is not null from pg_roles where rolname = 't1_role'")" "role can log in and has a password"
expect_eq "t1_role" "$(sql postgres "select pg_get_userbyid(datdba) from pg_database where datname = 't1'")" "database owner"
expect_eq "t1_role" "$(sql t1 "select tableowner from pg_tables where tablename = 'releases'")" "tables are owned by the role"
expect_eq "t1_role" "$(sql t1 "select tableowner from pg_tables where tablename = 'schema_migrations'")" "bookkeeping table owner"
expect_eq "1|0001_init.sql" "$(sql t1 "select version, name from schema_migrations where version = 1")" "recorded migration"
expect_eq "$N" "$(sql t1 "select count(*) from schema_migrations")" "all migrations recorded"
expect_eq "$N" "$(sql t1 "select count(*) from schema_migrations where length(checksum) = 64")" "checksums recorded"
expect_eq "t" "$(sql t1 "select count(*) > 10 from pg_tables where schemaname = 'public'")" "tables created"

# --- running it again changes nothing ------------------------------------------
out=$($DB bootstrap -U postgres -d t1 -r t1_role 2>&1); code=$?
expect_eq 0 "$code" "second bootstrap exit code"
expect_in "role t1_role exists (password left unchanged)" "$out" "role kept"
expect_in "database t1 exists" "$out" "database kept"
expect_in "database is up to date" "$out" "nothing to apply"
expect_in "the role already existed" "$out" "no credentials for an existing role"
expect_not_in "$password" "$out" "the old password must not reappear"
expect_not_in "wrote database_url" "$out" "an existing role's settings are left alone"
expect_in "t1_role:$password@" "$(cat "$SETTINGS")" "settings still hold the credentials"
expect_eq "$N" "$(sql t1 "select count(*) from schema_migrations")" "no duplicate bookkeeping rows"

# --- status ------------------------------------------------------------------------
out=$($DB status -U postgres -d t1 2>&1)
expect_in "applied  0001_init.sql" "$out" "status of an applied migration"

# --- a new migration is applied on its own ---------------------------------------
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cp migrations/*.sql "$tmp"/
echo "ALTER TABLE releases ADD COLUMN test_col text;" >"$tmp/${NEXT}_add_test_col.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB status -U postgres -d t1 2>&1)
expect_in "pending  ${NEXT}_add_test_col.sql" "$out" "status of a pending migration"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 0 "$code" "migrate exit code"
expect_in "applying ${NEXT}_add_test_col.sql" "$out" "new migration applied"
expect_not_in "applying 0001" "$out" "old migration not repeated"
expect_not_in "applying 0002" "$out" "old migration not repeated (2)"
expect_eq "test_col" "$(sql t1 "select column_name from information_schema.columns where table_name = 'releases' and column_name = 'test_col'")" "column added"
expect_eq $((N + 1)) "$(sql t1 "select count(*) from schema_migrations")" "bookkeeping rows"

# --- an edited migration is refused ----------------------------------------------
echo "-- edited later" >>"$tmp/0001_init.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 1 "$code" "edited migration exit code"
expect_in "was changed after it was applied" "$out" "edited migration message"
cp migrations/0001_init.sql "$tmp/0001_init.sql"

# --- a failing migration leaves nothing behind -----------------------------------
printf 'CREATE TABLE half_done (id integer);\nSELECT this_is_not_valid;\n' >"$tmp/${NEXT2}_broken.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 1 "$code" "failing migration exit code"
expect_in "migration ${NEXT2}_broken.sql failed" "$out" "failing migration message"
expect_eq "" "$(sql t1 "select to_regclass('half_done')")" "partial changes are rolled back"
expect_eq $((N + 1)) "$(sql t1 "select count(*) from schema_migrations")" "failed migration is not recorded"
printf 'CREATE TABLE half_done (id integer);\n' >"$tmp/${NEXT2}_broken.sql"
out=$(RECORDSHELF_MIGRATIONS_DIR=$tmp $DB migrate -U postgres -d t1 2>&1); code=$?
expect_eq 0 "$code" "fixed migration exit code"
expect_in "applying ${NEXT2}_broken.sql" "$out" "fixed migration is applied"

# --- password from the environment is used and never echoed ----------------------
out=$(RECORDSHELF_DB_PASSWORD=sup3rsecret $DB bootstrap -U postgres -d t2 -r t2_role 2>&1); code=$?
expect_eq 0 "$code" "bootstrap with a given password"
expect_not_in "sup3rsecret" "$out" "a given password is not echoed"
expect_in "wrote database_url to" "$out" "given password is stored too"
expect_in "t2_role:sup3rsecret@" "$(cat "$SETTINGS")" "given password in the settings file"
expect_eq 1 "$(grep -c '^database_url' "$SETTINGS")" "the new url replaces the old one"
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

# --- the settings file ------------------------------------------------------------
printf 'username = "me"\nzoom_factor = 1.5\n' >"$SETTINGS"
$DB bootstrap -U postgres -d s1 -r s1_role >/dev/null 2>&1
expect_in 'username = "me"' "$(cat "$SETTINGS")" "other settings are kept"
expect_in "zoom_factor = 1.5" "$(cat "$SETTINGS")" "other settings are kept (2)"
expect_in 'database_url = "postgres://s1_role:' "$(cat "$SETTINGS")" "url added to an existing file"

# characters that are special in a URL are encoded
out=$(RECORDSHELF_DB_PASSWORD='p@ss/w:rd#1 %' $DB bootstrap -U postgres -d s2 -r s2_role 2>&1)
expect_in "s2_role:p%40ss%2Fw%3Ard%231%20%25@" "$(cat "$SETTINGS")" "password is percent-encoded"
expect_not_in "p@ss" "$out" "special password is not printed"

# --no-settings leaves the file alone and shows the connection string once
before=$(sha256sum "$SETTINGS")
out=$($DB bootstrap -U postgres -d s3 -r s3_role --no-settings 2>&1)
expect_eq "$before" "$(sha256sum "$SETTINGS")" "--no-settings does not touch the file"
expect_in "connection string for the settings file" "$out" "--no-settings prints the url"
expect_in "postgres://s3_role:" "$out" "--no-settings shows the generated password"
expect_in "keep it" "$out" "--no-settings warns to keep the password"

# --settings chooses another file
other="$XDG_CONFIG_HOME/other/config.toml"
$DB bootstrap -U postgres -d s4 -r s4_role --settings "$other" >/dev/null 2>&1
expect_in 'database_url = "postgres://s4_role:' "$(cat "$other")" "--settings FILE"
expect_eq "600" "$(stat -c %a "$other")" "created file is private"
expect_not_in "s4_role" "$(cat "$SETTINGS")" "the default file was not touched"

# a value with quotes or backslashes cannot break the line
RECORDSHELF_DB_PASSWORD='q"u\ote' $DB bootstrap -U postgres -d s5 -r s5_role --settings "$other" >/dev/null 2>&1
expect_eq 1 "$(grep -c '^database_url' "$other")" "still one url line"
expect_in "s5_role:q%22u%5Cote@" "$(cat "$other")" "quote and backslash are encoded"

# dropping removes the matching url, keeps the rest, and mentions a different one
$DB drop -U postgres -d s1 -r s1_role --confirm s1 --settings "$other" >/dev/null 2>&1
expect_in "s5_role" "$(cat "$other")" "an url for another database stays"
out=$($DB drop -U postgres -d s5 -r s5_role --confirm s5 --settings "$other" 2>&1)
expect_in "removed database_url" "$out" "matching url removed on drop"
expect_not_in "database_url" "$(cat "$other")" "settings file no longer has the url"
printf 'username = "me"\ndatabase_url = "postgres://someone:pw@elsewhere/s2"\n' >"$other"
out=$($DB drop -U postgres -d s2 -r s2_role --confirm s2 --settings "$other" 2>&1)
expect_in "still points at s2" "$out" "note for an url that does not match the role"
expect_in "elsewhere/s2" "$(cat "$other")" "non-matching url is kept"
expect_in 'username = "me"' "$(cat "$other")" "other settings survive a drop"
out=$($DB drop -U postgres -d s3 -r s3_role --confirm s3 --no-settings --settings "$other" 2>&1)
expect_in "elsewhere/s2" "$(cat "$other")" "--no-settings leaves the file on drop"
for d in s1 s2 s3 s4 s5; do $DB drop -U postgres -d $d -r ${d}_role --confirm $d --no-settings >/dev/null 2>&1; done

# --- drop ------------------------------------------------------------------------
$DB bootstrap -U postgres -d d1 -r d1_role >/dev/null 2>&1
exists() { sql postgres "select 1 from pg_database where datname = '$1'"; }
role_exists() { sql postgres "select 1 from pg_roles where rolname = '$1'"; }

out=$($DB drop -U postgres -d d1 -r d1_role </dev/null 2>&1); code=$?
expect_eq 1 "$code" "drop without confirmation exit code"
expect_in "not confirmed" "$out" "drop without a terminal or --confirm"
expect_in "database d1" "$out" "drop shows what it would delete"
expect_eq 1 "$(exists d1)" "database survives an unconfirmed drop"
out=$($DB drop -U postgres -d d1 -r d1_role --confirm wrong 2>&1); code=$?
expect_eq 1 "$code" "drop with a wrong name exit code"
expect_in "nothing was deleted" "$out" "wrong name message"
expect_eq 1 "$(exists d1)" "database survives a wrong confirmation"
expect_eq 1 "$(role_exists d1_role)" "role survives a wrong confirmation"

# a session that is still connected does not prevent it
(psql -X -q -h "$PGHOST" -p "$PGPORT" -U postgres -d d1 -c "select pg_sleep(30)" >/dev/null 2>&1 &)
sleep 1
out=$($DB drop -U postgres -d d1 -r d1_role --confirm d1 2>&1); code=$?
expect_eq 0 "$code" "drop exit code"
expect_in "dropped database d1" "$out" "database dropped"
expect_in "dropped role d1_role" "$out" "role dropped"
expect_eq "" "$(exists d1)" "database is gone"
expect_eq "" "$(role_exists d1_role)" "role is gone"

out=$($DB drop -U postgres -d d1 -r d1_role --confirm d1 2>&1); code=$?
expect_eq 0 "$code" "drop of nothing exit code"
expect_in "nothing to do" "$out" "drop twice"

# after a drop the bootstrap starts from scratch
out=$($DB bootstrap -U postgres -d d1 -r d1_role 2>&1); code=$?
expect_eq 0 "$code" "bootstrap after drop"
expect_in "created role d1_role" "$out" "role is created again"
expect_in "applying 0001_init.sql" "$out" "migrations run again"

# --keep-role
out=$($DB drop -U postgres -d d1 -r d1_role --confirm d1 --keep-role 2>&1)
expect_in "kept role d1_role" "$out" "role kept on request"
expect_eq 1 "$(role_exists d1_role)" "kept role still exists"
expect_eq "" "$(exists d1)" "database dropped even when the role is kept"
$DB drop -U postgres -d d1 -r d1_role --confirm d1 >/dev/null 2>&1
expect_eq "" "$(role_exists d1_role)" "a later drop removes the leftover role"

# a role that owns another database is not dropped
$DB bootstrap -U postgres -d d2 -r shared_role >/dev/null 2>&1
sql postgres "create database d3 owner shared_role" >/dev/null
out=$($DB drop -U postgres -d d2 -r shared_role --confirm d2 2>&1)
expect_in "also owns the database(s) d3" "$out" "shared role note"
expect_in "dropped database d2" "$out" "first database dropped"
expect_eq 1 "$(role_exists shared_role)" "shared role is kept"
expect_eq 1 "$(exists d3)" "the other database is untouched"

# guard rails
for system_db in postgres template1; do
    out=$($DB drop -U postgres -d $system_db --confirm $system_db 2>&1); code=$?
    expect_eq 1 "$code" "dropping $system_db exit code"
    expect_in "refusing to drop the system database" "$out" "system database $system_db"
done
expect_eq 1 "$(exists postgres)" "the postgres database is still there"
out=$($DB drop -U t1_role -d t1 --confirm t1 2>&1); code=$?
expect_eq 1 "$code" "drop as non-superuser exit code"
expect_in "needs a superuser" "$out" "drop as non-superuser"
expect_eq 1 "$(exists t1)" "database survives a non-superuser drop"
out=$($DB drop -U postgres -d 'x;y' --confirm 'x;y' 2>&1)
expect_in "invalid database name" "$out" "drop with a bad name"

# the interactive prompt, if a pseudo terminal can be made
if command -v script >/dev/null 2>&1; then
    $DB bootstrap -U postgres -d d4 -r d4_role >/dev/null 2>&1
    out=$(printf 'nope\n' | script -qec "$DB drop -U postgres -d d4 -r d4_role" /dev/null 2>&1)
    expect_in "type the database name (d4) to confirm" "$out" "interactive prompt"
    expect_eq 1 "$(exists d4)" "a wrong answer at the prompt deletes nothing"
    out=$(printf 'd4\n' | script -qec "$DB drop -U postgres -d d4 -r d4_role" /dev/null 2>&1)
    expect_eq "" "$(exists d4)" "the right answer at the prompt deletes the database"
fi

out=$($DB --help 2>&1)
expect_in "db/db.sh bootstrap" "$out" "help text"

echo "bootstrap tests: $((checks - failures))/$checks passed"
[ "$failures" -eq 0 ]
