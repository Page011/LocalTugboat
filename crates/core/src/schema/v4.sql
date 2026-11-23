-- Tugboat schema version 4. Forward only; the runner copies the v3 file to
-- tugboat.db.bak-3 first (db.rs rule 4), applies this in one transaction, runs
-- PRAGMA foreign_key_check and PRAGMA integrity_check inside it, and sets user_version = 4.
--
-- 1. item.purged_at / purged_lamport / purged_dev: the "purged" group, deleted forever. All three
--    are NULL until a device deletes the item forever; once set, nothing clears them. Every row
--    is kept, none is purged by this migration.
ALTER TABLE item ADD COLUMN purged_at INTEGER;
ALTER TABLE item ADD COLUMN purged_lamport INTEGER CHECK (purged_lamport IS NULL OR purged_lamport >= 1);
ALTER TABLE item ADD COLUMN purged_dev BLOB CHECK (purged_dev IS NULL OR length(purged_dev) = 32);
CREATE INDEX item_purged ON item (purged_at) WHERE purged_at IS NOT NULL;

-- 2. thumb_trash (local): thumbnails to delete from disk once no live item uses them. A purge
--    adds the hash; the core drains the table after every sweep. A crash between the commit and
--    the file delete leaves the row, so the thumbnail is removed on the next run.
CREATE TABLE thumb_trash (
    hash  BLOB PRIMARY KEY CHECK (length(hash) = 32)
) STRICT;
