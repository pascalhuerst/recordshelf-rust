-- Checks of the schema's rules. Run against a database that has all
-- migrations applied (see db/test.sh); changes are rolled back.
\set ON_ERROR_STOP on
BEGIN;

-- Asserts that a statement is rejected with the given SQLSTATE.
CREATE FUNCTION expect_error(stmt text, state text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE stmt;
    RAISE EXCEPTION 'expected error % but statement succeeded: %', state, stmt;
EXCEPTION WHEN OTHERS THEN
    IF SQLSTATE <> state THEN
        RAISE EXCEPTION 'expected % but got % (%): %', state, SQLSTATE, SQLERRM, stmt;
    END IF;
END
$$;

-- a complete release, as the app will write it
INSERT INTO releases (id, master_id, title, released_on, released_precision, country, genres, styles)
VALUES (10, 99, 'Speicher 95 Tribute EP', '2017-03-01', 'month', 'DE', '{Electronic}', '{Techno}');
INSERT INTO artists (id, name) VALUES (1, 'Laurent Garnier'), (2, 'Someone');
INSERT INTO release_artists (release_id, position, artist_id, join_text)
VALUES (10, 1, 1, 'feat.'), (10, 2, 2, NULL);
INSERT INTO labels (id, name) VALUES (5, 'Kompakt Extra');
INSERT INTO release_labels (release_id, label_id, catalog_number) VALUES (10, 5, 'Kompakt Ex 095');
INSERT INTO release_formats (release_id, position, size, rpm, weight_g, text, descriptions)
VALUES (10, 1, 12, 33, 180, 'X-ray Colour', '{EP}');
INSERT INTO tracks (release_id, track_index, position, title, duration_seconds)
VALUES (10, 0, 'A', 'Doctor C''est Chouette', 380);
INSERT INTO track_artists (track_id, position, artist_id) SELECT id, 1, 2 FROM tracks;
INSERT INTO release_credits (release_id, artist_id, role, tracks) VALUES (10, 2, 'Producer', 'A');
INSERT INTO release_covers (release_id, image, thumb, mime) VALUES (10, '\x00', '\x00', 'image/jpeg');

DO $$
BEGIN
    -- defaults
    IF (SELECT copies FROM releases WHERE id = 10) <> 1 THEN RAISE EXCEPTION 'copies should default to 1'; END IF;
    IF (SELECT channels FROM release_formats WHERE release_id = 10) <> 'stereo' THEN
        RAISE EXCEPTION 'channels should default to stereo';
    END IF;
    IF (SELECT qty FROM release_formats WHERE release_id = 10) <> 1 THEN RAISE EXCEPTION 'qty should default to 1'; END IF;
    IF (SELECT created_at FROM releases WHERE id = 10) IS NULL THEN RAISE EXCEPTION 'created_at not set'; END IF;
    -- generated year
    IF (SELECT year FROM releases WHERE id = 10) <> 2017 THEN RAISE EXCEPTION 'year not derived from released_on'; END IF;
    IF (SELECT year FROM releases WHERE id = 10) IS NULL THEN RAISE EXCEPTION 'year missing'; END IF;
END
$$;

-- updated_at moves on update, created_at does not
UPDATE releases SET created_at = now() - interval '1 day', updated_at = now() - interval '1 day' WHERE id = 10;
UPDATE releases SET title = 'Speicher 95 Tribute EP (renamed)' WHERE id = 10;
DO $$
BEGIN
    IF (SELECT updated_at <= now() - interval '1 hour' FROM releases WHERE id = 10) THEN
        RAISE EXCEPTION 'updated_at was not refreshed by the trigger';
    END IF;
    IF (SELECT created_at > now() - interval '1 hour' FROM releases WHERE id = 10) THEN
        RAISE EXCEPTION 'created_at must not change on update';
    END IF;
END
$$;

-- rules
\o /dev/null
SELECT expect_error($$INSERT INTO releases (id, title, copies) VALUES (11, 'x', 0)$$, '23514');
SELECT expect_error($$INSERT INTO releases (id, title, released_on) VALUES (11, 'x', '2017-01-01')$$, '23514'); -- date without precision
SELECT expect_error($$INSERT INTO releases (id, title, released_precision) VALUES (11, 'x', 'year')$$, '23514'); -- precision without date
SELECT expect_error($$INSERT INTO releases (id, title) VALUES (10, 'duplicate id')$$, '23505');
SELECT expect_error($$INSERT INTO releases (id, title) VALUES (12, NULL)$$, '23502');
SELECT expect_error($$INSERT INTO release_formats (release_id, position, size) VALUES (10, 2, 5)$$, '23514');
SELECT expect_error($$INSERT INTO release_formats (release_id, position, rpm) VALUES (10, 2, 16)$$, '23514');
SELECT expect_error($$INSERT INTO release_formats (release_id, position, weight_g) VALUES (10, 2, 0)$$, '23514');
SELECT expect_error($$INSERT INTO release_formats (release_id, position, qty) VALUES (10, 2, 0)$$, '23514');
SELECT expect_error($$INSERT INTO release_formats (release_id, position, channels) VALUES (10, 2, 'surround')$$, '22P02');
SELECT expect_error($$INSERT INTO release_formats (release_id, position) VALUES (10, 1)$$, '23505'); -- same position twice
SELECT expect_error($$INSERT INTO release_artists (release_id, position, artist_id) VALUES (10, 3, 1)$$, '23505'); -- same artist twice
SELECT expect_error($$INSERT INTO release_artists (release_id, position, artist_id) VALUES (10, 3, 404)$$, '23503'); -- unknown artist
SELECT expect_error($$INSERT INTO release_artists (release_id, position, artist_id) VALUES (404, 1, 1)$$, '23503'); -- unknown release
SELECT expect_error($$INSERT INTO tracks (release_id, track_index, title) VALUES (10, 0, 'same index')$$, '23505');
SELECT expect_error($$INSERT INTO tracks (release_id, track_index, title, duration_seconds) VALUES (10, 1, 'x', -5)$$, '23514');
SELECT expect_error($$INSERT INTO release_labels (release_id, label_id, catalog_number) VALUES (10, 5, 'Kompakt Ex 095')$$, '23505');

\o

-- the same label with a second catalog number, and a release without one, are fine
INSERT INTO release_labels (release_id, label_id, catalog_number) VALUES (10, 5, 'Alt 1');
INSERT INTO release_labels (release_id, label_id) VALUES (10, 5);
-- several formats in one release
INSERT INTO release_formats (release_id, position, qty, size) VALUES (10, 2, 1, 7);

-- deleting a release removes everything that belongs to it, but not the shared artists/labels
DELETE FROM releases WHERE id = 10;
DO $$
DECLARE leftovers integer;
BEGIN
    SELECT (SELECT count(*) FROM release_artists) + (SELECT count(*) FROM release_labels)
         + (SELECT count(*) FROM release_formats) + (SELECT count(*) FROM tracks)
         + (SELECT count(*) FROM track_artists) + (SELECT count(*) FROM release_credits)
         + (SELECT count(*) FROM release_covers) INTO leftovers;
    IF leftovers <> 0 THEN RAISE EXCEPTION '% rows survived deleting the release', leftovers; END IF;
    IF (SELECT count(*) FROM artists) <> 2 OR (SELECT count(*) FROM labels) <> 1 THEN
        RAISE EXCEPTION 'artists and labels must be kept';
    END IF;
END
$$;

ROLLBACK;
\echo schema checks passed
