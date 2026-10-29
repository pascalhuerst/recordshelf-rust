-- RecordShelf catalogue: every row in `releases` is a record we own, taken
-- from Discogs. Identifiers of releases, artists and labels are the Discogs
-- ids. See db/README.md for the conventions.

CREATE TYPE date_precision AS ENUM ('year', 'month', 'day');

-- Discogs also lists "Quadraphonic". Most pressings do not say mono or
-- stereo at all; those count as stereo.
CREATE TYPE channels AS ENUM ('mono', 'stereo', 'quadraphonic');

CREATE TABLE releases (
    id                 integer PRIMARY KEY,           -- Discogs release id
    master_id          integer,                       -- Discogs master release: groups pressings of one album
    title              text NOT NULL,
    -- Discogs dates can be partial ("2017", "2017-03"). A partial date is
    -- stored as its first day, released_precision says how much is known.
    released_on        date,
    released_precision date_precision,
    year               smallint GENERATED ALWAYS AS (EXTRACT(YEAR FROM released_on)::smallint) STORED,
    country            text,
    genres             text[] NOT NULL DEFAULT '{}',
    styles             text[] NOT NULL DEFAULT '{}',
    notes              text,                          -- Discogs release notes (pressing info)
    discogs_uri        text,
    copies             smallint NOT NULL DEFAULT 1 CHECK (copies >= 1),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),  -- last sync with Discogs
    CHECK ((released_on IS NULL) = (released_precision IS NULL))
);

CREATE INDEX releases_master_id_idx ON releases (master_id);
CREATE INDEX releases_year_idx ON releases (year);

CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END
$$;

CREATE TRIGGER releases_set_updated_at
    BEFORE UPDATE ON releases
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Our own copy of the cover (full size and a small thumbnail for lists).
CREATE TABLE release_covers (
    release_id integer PRIMARY KEY REFERENCES releases (id) ON DELETE CASCADE,
    image      bytea NOT NULL,
    thumb      bytea NOT NULL,
    mime       text NOT NULL,
    source_url text,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE artists (
    id   integer PRIMARY KEY,                         -- Discogs artist id
    name text NOT NULL
);

CREATE INDEX artists_name_idx ON artists (lower(name));

-- The artists of a release in sleeve order. join_text is what the sleeve puts
-- between this artist and the next one ("&", ",", "feat.", "vs."); NULL for
-- the last artist.
CREATE TABLE release_artists (
    release_id integer NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    position   smallint NOT NULL CHECK (position >= 1),
    artist_id  integer NOT NULL REFERENCES artists (id),
    join_text  text,
    PRIMARY KEY (release_id, position),
    UNIQUE (release_id, artist_id)
);

CREATE INDEX release_artists_artist_idx ON release_artists (artist_id);

CREATE TABLE labels (
    id   integer PRIMARY KEY,                         -- Discogs label id
    name text NOT NULL
);

CREATE INDEX labels_name_idx ON labels (lower(name));

-- A release can appear on several labels, or on one label with several
-- catalog numbers; '' means no catalog number.
CREATE TABLE release_labels (
    release_id     integer NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    label_id       integer NOT NULL REFERENCES labels (id),
    catalog_number text NOT NULL DEFAULT '',
    PRIMARY KEY (release_id, label_id, catalog_number)
);

CREATE INDEX release_labels_label_idx ON release_labels (label_id);
CREATE INDEX release_labels_catalog_number_idx ON release_labels (lower(catalog_number));

-- Everything is vinyl, so only the physical details are kept. One row per
-- format of a release (a box set can hold a 12" and a 7").
CREATE TABLE release_formats (
    release_id   integer NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    position     smallint NOT NULL CHECK (position >= 1),
    qty          smallint NOT NULL DEFAULT 1 CHECK (qty >= 1),     -- number of discs
    size         smallint CHECK (size IN (7, 10, 12)),             -- inch; NULL if Discogs does not say
    rpm          smallint CHECK (rpm IN (33, 45, 78)),
    channels     channels NOT NULL DEFAULT 'stereo',
    weight_g     smallint CHECK (weight_g > 0),                    -- grams, e.g. 180
    text         text,                                             -- free text, e.g. "X-ray Colour"
    descriptions text[] NOT NULL DEFAULT '{}',                     -- the remaining tags: EP, Reissue, ...
    PRIMARY KEY (release_id, position)
);

CREATE INDEX release_formats_size_idx ON release_formats (size);
CREATE INDEX release_formats_weight_idx ON release_formats (weight_g);
CREATE INDEX release_formats_descriptions_idx ON release_formats USING gin (descriptions);

CREATE TABLE tracks (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    release_id       integer NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    track_index      integer NOT NULL CHECK (track_index >= 0),    -- order within the release
    position         text,                                         -- as printed: A1, B2, 1, ...
    title            text NOT NULL,
    duration_seconds integer CHECK (duration_seconds >= 0),
    UNIQUE (release_id, track_index)
);

-- Track artists, only given by Discogs where they differ from the release
-- artists (compilations, split releases).
CREATE TABLE track_artists (
    track_id  bigint NOT NULL REFERENCES tracks (id) ON DELETE CASCADE,
    position  smallint NOT NULL CHECK (position >= 1),
    artist_id integer NOT NULL REFERENCES artists (id),
    join_text text,
    PRIMARY KEY (track_id, position)
);

CREATE INDEX track_artists_artist_idx ON track_artists (artist_id);

-- Further credits: producer, remixer, mastering, ...
CREATE TABLE release_credits (
    id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    release_id integer NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    artist_id  integer NOT NULL REFERENCES artists (id),
    role       text NOT NULL,
    tracks     text                                  -- free text from Discogs, e.g. "A1, B2"
);

CREATE INDEX release_credits_release_idx ON release_credits (release_id);
CREATE INDEX release_credits_artist_idx ON release_credits (artist_id);
