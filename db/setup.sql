-- One-time setup of the RecordShelf database. Run as a Postgres superuser:
--
--   psql -U postgres -v password="'choose-a-password'" -f db/setup.sql
--
-- (the quotes inside the variable matter). The tables are created afterwards
-- by the migrations in db/migrations, applied with the app's migration
-- runner or by hand with psql.

CREATE ROLE recordshelf LOGIN PASSWORD :password;

CREATE DATABASE recordshelf OWNER recordshelf ENCODING 'UTF8';
