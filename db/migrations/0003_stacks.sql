-- Stacks: records are labelled one after another and put back into a box in
-- that order. A stack is such a box (or pile), named by us; the position of a
-- record in it is the order it was labelled in. Later the order is used to
-- put the records on the shelf ("7th record from stack ABC").
CREATE TABLE stacks (
    id         serial PRIMARY KEY,
    name       text NOT NULL CHECK (btrim(name) <> ''),
    -- the stack newly labelled records go on; at most one
    active     boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX stacks_name_idx ON stacks (lower(name));
CREATE UNIQUE INDEX stacks_one_active_idx ON stacks (active) WHERE active;

-- Positions are 1, 2, 3 ... without gaps (renumbered when a record is taken
-- out or moved), except after a release was deleted from the library: then
-- there is a gap, the order is still right. A record may be on several
-- stacks, but only once on each.
CREATE TABLE stack_records (
    stack_id   integer NOT NULL REFERENCES stacks (id) ON DELETE CASCADE,
    position   integer NOT NULL CHECK (position >= 1),
    release_id integer NOT NULL REFERENCES releases (id) ON DELETE CASCADE,
    added_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (stack_id, position) DEFERRABLE INITIALLY DEFERRED,
    UNIQUE (stack_id, release_id)
);

CREATE INDEX stack_records_release_idx ON stack_records (release_id);
