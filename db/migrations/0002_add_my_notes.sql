-- Our own notes about a record. releases.notes holds the Discogs release
-- notes (pressing information) and is refreshed by every sync; my_notes is
-- ours and is never touched by a sync.
ALTER TABLE releases ADD COLUMN my_notes text;
