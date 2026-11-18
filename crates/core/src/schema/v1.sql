-- Tugboat schema version 1. Same schema on phone and PC.
-- Conventions: times are unix ms (INTEGER); ids are BLOBs (uuid 16 bytes, keys/hashes 32
-- bytes); each synced field group has <group>_lamport + <group>_dev (the Version).
-- Local-only columns/tables are marked "local". Applied inside one transaction, then
-- PRAGMA user_version = 1 (the runner sets it, not this file).

CREATE TABLE device (
    id            BLOB PRIMARY KEY CHECK (length(id) = 32),   -- ed25519 public key
    name          TEXT NOT NULL,
    is_self       INTEGER NOT NULL CHECK (is_self IN (0, 1)),
    role          TEXT NOT NULL CHECK (role IN ('pc', 'phone')),
    paired_at     INTEGER,
    unpaired_at   INTEGER,
    last_seen_at  INTEGER,
    addresses     TEXT NOT NULL DEFAULT '[]'                  -- JSON array of "ip:port"
) STRICT;
CREATE UNIQUE INDEX device_one_self ON device (is_self) WHERE is_self = 1;

-- Lamport clock of this device: max version lamport seen or issued. One row, id = 1.
CREATE TABLE lamport (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    value  INTEGER NOT NULL CHECK (value >= 0)
) STRICT;
INSERT INTO lamport (id, value) VALUES (1, 0);

CREATE TABLE folder (
    id                 BLOB PRIMARY KEY CHECK (length(id) = 16),
    kind               TEXT NOT NULL CHECK (kind IN ('inbox', 'keep', 'unsorted', 'user')),
    name               TEXT NOT NULL,
    name_lamport       INTEGER NOT NULL,
    name_dev           BLOB NOT NULL CHECK (length(name_dev) = 32),
    retention_secs     INTEGER CHECK (retention_secs IS NULL OR retention_secs > 0), -- NULL = Keep
    retention_lamport  INTEGER NOT NULL,
    retention_dev      BLOB NOT NULL CHECK (length(retention_dev) = 32),
    deleted_at         INTEGER,
    deleted_lamport    INTEGER NOT NULL,
    deleted_dev        BLOB NOT NULL CHECK (length(deleted_dev) = 32)
) STRICT;

CREATE TABLE place (
    id               BLOB PRIMARY KEY CHECK (length(id) = 16),
    name             TEXT NOT NULL,
    name_lamport     INTEGER NOT NULL,
    name_dev         BLOB NOT NULL CHECK (length(name_dev) = 32),
    lat              REAL NOT NULL CHECK (lat BETWEEN -90 AND 90),
    lon              REAL NOT NULL CHECK (lon BETWEEN -180 AND 180),
    radius_m         REAL NOT NULL CHECK (radius_m > 0),
    area_lamport     INTEGER NOT NULL,
    area_dev         BLOB NOT NULL CHECK (length(area_dev) = 32),
    folder_id        BLOB NOT NULL CHECK (length(folder_id) = 16), -- no FK: folder may arrive later
    target_lamport   INTEGER NOT NULL,
    target_dev       BLOB NOT NULL CHECK (length(target_dev) = 32),
    deleted_at       INTEGER,
    deleted_lamport  INTEGER NOT NULL,
    deleted_dev      BLOB NOT NULL CHECK (length(deleted_dev) = 32)
) STRICT;

CREATE TABLE item (
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
    media_kind          TEXT NOT NULL CHECK (media_kind IN ('image', 'video')),
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
CREATE INDEX item_folder   ON item (folder_id, captured_at DESC, id DESC);
CREATE INDEX item_captured ON item (captured_at DESC, id DESC);
CREATE INDEX item_hash     ON item (blob_hash);
CREATE INDEX item_local    ON item (local_file_state);
CREATE INDEX item_entered  ON item (folder_entered_at);

-- Grow-only set: device holds (or held) the verified file with this hash.
CREATE TABLE receipt (
    blob_hash    BLOB NOT NULL CHECK (length(blob_hash) = 32),
    device_id    BLOB NOT NULL CHECK (length(device_id) = 32),
    received_at  INTEGER NOT NULL,     -- synced: on device_id's clock
    seen_at      INTEGER NOT NULL,     -- local: when this device stored the row
    PRIMARY KEY (blob_hash, device_id)
) STRICT, WITHOUT ROWID;

-- local: one row per local write of a synced field group (and per new receipt).
-- Remote-applied changes are NOT logged (the peer already has them).
CREATE TABLE change_log (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    entity       TEXT NOT NULL CHECK (entity IN ('item', 'folder', 'place', 'receipt')),
    entity_id    BLOB NOT NULL,        -- item/folder/place id, or blob_hash || device_id for receipts
    field_group  TEXT NOT NULL,
    lamport      INTEGER NOT NULL
) STRICT;
CREATE INDEX change_log_entity ON change_log (entity, entity_id, field_group);

-- local: sync position with the (one) peer.
CREATE TABLE peer_cursor (
    peer_id          BLOB PRIMARY KEY CHECK (length(peer_id) = 32),
    last_seq_pulled  INTEGER NOT NULL,  -- peer's change_log seq we have applied up to
    our_seq_acked    INTEGER NOT NULL DEFAULT 0  -- our seq the peer asked "since" last
) STRICT;

-- local: history shown on the Activity screen, trimmed after 30 days.
CREATE TABLE activity (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    at         INTEGER NOT NULL,
    device_id  BLOB,
    item_id    BLOB,
    action     TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX activity_at ON activity (at);

-- local: where this device holds an item's file. PC adds the NTFS identity for move matching.
CREATE TABLE file_ref (
    item_id        BLOB PRIMARY KEY CHECK (length(item_id) = 16),
    path           TEXT NOT NULL,
    volume_serial  INTEGER,            -- PC only
    file_id        BLOB                -- PC only: FILE_ID_128 (16 bytes)
) STRICT;
CREATE UNIQUE INDEX file_ref_ntfs ON file_ref (volume_serial, file_id) WHERE file_id IS NOT NULL;
CREATE INDEX file_ref_path ON file_ref (path);

-- local: settings (key -> JSON value). Missing key = default from model::Settings.
CREATE TABLE setting (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
) STRICT, WITHOUT ROWID;
