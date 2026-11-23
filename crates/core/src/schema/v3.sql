-- Tugboat schema version 3. Forward only; the runner copies the v2 file to
-- tugboat.db.bak-2 first (db.rs rule 4), applies this in one transaction, runs
-- PRAGMA foreign_key_check and PRAGMA integrity_check inside it, and sets user_version = 3.
--
-- 1. change_log.src (local): the device a change was taken from. NULL = written on this device
--    (local op, own receipt, delete-vs-keep rewrite). Only the phone writes non-NULL rows, when it
--    relays what one PC sent to the others.
ALTER TABLE change_log ADD COLUMN src BLOB CHECK (src IS NULL OR length(src) = 32);

-- 2. Per-device last synced time (local). It replaces the single 'last_synced_at' setting.
ALTER TABLE device ADD COLUMN last_synced_at INTEGER;

UPDATE device
   SET last_synced_at = (SELECT CAST(value AS INTEGER) FROM setting
                          WHERE key = 'last_synced_at' AND value GLOB '[0-9]*')
 WHERE is_self = 0 AND paired_at IS NOT NULL AND unpaired_at IS NULL;
DELETE FROM setting WHERE key = 'last_synced_at';

-- 3. log_floor (local): every change_log row with seq <= log_floor may be gone. Start
--    conservative: the row before the oldest row held, or the head when the log is empty.
--    Overstating costs one extra snapshot; understating would lose updates.
INSERT INTO setting (key, value)
SELECT 'log_floor',
       CAST(COALESCE((SELECT MIN(seq) - 1 FROM change_log),
                     (SELECT seq FROM sqlite_sequence WHERE name = 'change_log'),
                     0) AS TEXT)
 WHERE NOT EXISTS (SELECT 1 FROM setting WHERE key = 'log_floor');
