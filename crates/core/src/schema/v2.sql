-- Tugboat schema version 2 (0.2.0). Forward only; the runner copies the v1 file to
-- tugboat.db.bak-1 first (db.rs rule 4), applies this in one transaction, runs
-- PRAGMA foreign_key_check and PRAGMA integrity_check inside it, and sets user_version = 2.
--
-- 1. Any file type syncs: item.media_kind also accepts 'file'. SQLite cannot alter a CHECK, so
--    the item table is rebuilt. Explicit column lists on both sides, never SELECT *: a column
--    added or reordered later must fail loudly here, not shift data silently.
--    No table has a foreign key to item, so foreign_keys=ON cannot cascade anything.
-- 2. file_ref.mtime_ms: the modified time of the held file when the PC watcher last saw it
--    (local, PC only). Lets a same-size edit made while the app was closed be noticed.

CREATE TABLE item_v2 (
    id                  BLOB PRIMARY KEY CHECK (length(id) = 16),  -- UUIDv7
    -- immutable
    captured_at         INTEGER NOT NULL,
    origin_device       BLOB NOT NULL CHECK (length(origin_device) = 32),
    lat                 REAL,
    lon                 REAL,
    accuracy_m          REAL,
    -- group: name
    name                TEXT NOT NULL,
    name_lamport        INTEGER NOT NULL,
    name_dev            BLOB NOT NULL CHECK (length(name_dev) = 32),
    -- group: content
    blob_hash           BLOB NOT NULL CHECK (length(blob_hash) = 32),
    size                INTEGER NOT NULL CHECK (size >= 0),
    media_kind          TEXT NOT NULL CHECK (media_kind IN ('image', 'video', 'file')),
    ext                 TEXT NOT NULL,
    thumb_hash          BLOB CHECK (thumb_hash IS NULL OR length(thumb_hash) = 32),
    thumb_size          INTEGER,
    content_lamport     INTEGER NOT NULL,
    content_dev         BLOB NOT NULL CHECK (length(content_dev) = 32),
    -- group: placement
    folder_id           BLOB NOT NULL CHECK (length(folder_id) = 16), -- no FK: folder may arrive later
    folder_entered_at   INTEGER NOT NULL,
    placement_lamport   INTEGER NOT NULL,
    placement_dev       BLOB NOT NULL CHECK (length(placement_dev) = 32),
    -- group: pinned
    pinned              INTEGER NOT NULL CHECK (pinned IN (0, 1)),
    pinned_lamport      INTEGER NOT NULL,
    pinned_dev          BLOB NOT NULL CHECK (length(pinned_dev) = 32),
    -- group: deleted
    deleted_at          INTEGER,
    deleted_lamport     INTEGER NOT NULL,
    deleted_dev         BLOB NOT NULL CHECK (length(deleted_dev) = 32),
    -- local
    local_file_state    TEXT NOT NULL CHECK (local_file_state IN
                          ('pending', 'present', 'binned', 'purged', 'lost', 'refused')),
    local_state_at      INTEGER NOT NULL
) STRICT;

INSERT INTO item_v2 (
    id, captured_at, origin_device, lat, lon, accuracy_m,
    name, name_lamport, name_dev,
    blob_hash, size, media_kind, ext, thumb_hash, thumb_size, content_lamport, content_dev,
    folder_id, folder_entered_at, placement_lamport, placement_dev,
    pinned, pinned_lamport, pinned_dev,
    deleted_at, deleted_lamport, deleted_dev,
    local_file_state, local_state_at
) SELECT
    id, captured_at, origin_device, lat, lon, accuracy_m,
    name, name_lamport, name_dev,
    blob_hash, size, media_kind, ext, thumb_hash, thumb_size, content_lamport, content_dev,
    folder_id, folder_entered_at, placement_lamport, placement_dev,
    pinned, pinned_lamport, pinned_dev,
    deleted_at, deleted_lamport, deleted_dev,
    local_file_state, local_state_at
FROM item;

DROP TABLE item;
ALTER TABLE item_v2 RENAME TO item;

CREATE INDEX item_folder   ON item (folder_id, captured_at DESC, id DESC);
CREATE INDEX item_captured ON item (captured_at DESC, id DESC);
CREATE INDEX item_hash     ON item (blob_hash);
CREATE INDEX item_local    ON item (local_file_state);
CREATE INDEX item_entered  ON item (folder_entered_at);

ALTER TABLE file_ref ADD COLUMN mtime_ms INTEGER;  -- local, PC only: file modified time (unix ms)
