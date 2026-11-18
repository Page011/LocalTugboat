//! Opening the database and forward-only migrations. Owner: logic builder.
//!
//! Rules (PLAN "Schema changes and rollback"):
//! 1. Schema version lives in `PRAGMA user_version`. `MIGRATIONS[i]` takes version i to i+1.
//! 2. Opening a file whose user_version > SCHEMA_VERSION -> `CoreError::NewerDatabase`,
//!    the file is not touched (no pragma writes, no WAL switch before the check).
//! 3. user_version 0 on a file with no tables = new database: apply all migrations, no backup.
//!    user_version 0 on a file WITH tables -> `Migration` error (unknown file, never guess).
//! 4. 0 < user_version < SCHEMA_VERSION: first write a consistent copy with
//!    `VACUUM INTO '<dir>/tugboat.db.bak-<old version>'` (includes WAL content; fails if
//!    the backup file already exists -> keep the older backup, use `.bak-<v>-<unix ms>`),
//!    then apply each pending migration in its own transaction, setting user_version inside
//!    the same transaction. Before the commit every step runs `PRAGMA foreign_key_check` and
//!    `PRAGMA integrity_check` and fails on any row/anything but "ok" (a table rebuild must never
//!    commit damaged data). Any failure -> `Migration{from,to,msg}`; the transaction rolls back
//!    and the db stays at the last good version. A backup that cannot be written (no space)
//!    fails the migration before anything is touched: the db stays at the old version.
//! 5. Connection pragmas after the version check: journal_mode=WAL, synchronous=NORMAL,
//!    foreign_keys=ON, busy_timeout=5000.
//! 6. After migrating, ensure the default folders exist (`folders::ensure_defaults`) and the
//!    self device row exists (done by the facade at open, which knows the key and name).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, TransactionBehavior};

use crate::{clock::Clock, CoreError, Result};

/// Highest schema version this build knows.
pub const SCHEMA_VERSION: u32 = 2;

/// Migration SQL, index i = from version i to i + 1.
pub const MIGRATIONS: &[&str] = &[include_str!("schema/v1.sql"), include_str!("schema/v2.sql")];

/// File name of the database inside the data dir (%LOCALAPPDATA%\Local Tugboat on the PC,
/// filesDir on Android).
pub const DB_FILE: &str = "tugboat.db";

pub struct Db {
    pub conn: rusqlite::Connection,
    /// None for in-memory test databases.
    pub path: Option<PathBuf>,
}

fn read_version(conn: &Connection) -> Result<u32> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(v.max(0) as u32)
}

fn has_tables(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn set_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

/// After a migration step, before its commit: foreign keys hold and SQLite finds the file sound.
fn check_integrity(conn: &Connection) -> rusqlite::Result<()> {
    let fk_rows = conn.prepare("PRAGMA foreign_key_check")?.query_map([], |_| Ok(()))?.count();
    if fk_rows > 0 {
        return Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY), Some(format!("foreign_key_check found {fk_rows} broken row(s)"))));
    }
    let report: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if report != "ok" {
        return Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT), Some(format!("integrity_check: {report}"))));
    }
    Ok(())
}

/// Apply `migrations[from..]` one transaction each. `from` is the current user_version.
fn run_migrations(conn: &mut Connection, migrations: &[&str], from: u32) -> Result<()> {
    for (i, sql) in migrations.iter().enumerate().skip(from as usize) {
        let (f, t) = (i as u32, i as u32 + 1);
        let step = |conn: &mut Connection| -> rusqlite::Result<()> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(sql)?;
            check_integrity(&tx)?;
            tx.pragma_update(None, "user_version", t)?;
            tx.commit()
        };
        step(conn).map_err(|e| CoreError::Migration { from: f, to: t, msg: e.to_string() })?;
    }
    Ok(())
}

/// Rule 4: consistent copy of the db at the old version. Never overwrites an older backup.
fn backup(conn: &Connection, data_dir: &Path, version: u32, now: i64) -> Result<()> {
    let mut target = backup_path(data_dir, version);
    if target.exists() {
        // A backup of this version exists from an earlier attempt: keep it, it is older.
        target = data_dir.join(format!("{DB_FILE}.bak-{version}-{now}"));
    }
    let t = target.to_string_lossy().into_owned();
    conn.execute("VACUUM INTO ?1", [t])
        .map_err(|e| CoreError::Migration { from: version, to: version + 1, msg: format!("backup failed: {e}") })?;
    Ok(())
}

impl Db {
    /// Open or create `<data_dir>/tugboat.db` following rules 1-5.
    pub fn open(data_dir: &Path) -> Result<Db> {
        Db::open_with(data_dir, MIGRATIONS)
    }

    /// `open` with an explicit migration list (the tests use a synthetic two-step list).
    pub(crate) fn open_with(data_dir: &Path, migrations: &[&str]) -> Result<Db> {
        Db::open_inner(data_dir, migrations, true, crate::clock::SystemClock.now_ms())
    }

    fn open_inner(data_dir: &Path, migrations: &[&str], defaults: bool, now_ms: i64) -> Result<Db> {
        std::fs::create_dir_all(data_dir).map_err(|e| CoreError::io(data_dir, e))?;
        let path = data_dir.join(DB_FILE);
        let mut conn = Connection::open(&path)?;
        let supported = migrations.len() as u32;
        let found = read_version(&conn)?;
        if found > supported {
            // Rule 2: nothing has been written to the file at this point.
            return Err(CoreError::NewerDatabase { found, supported });
        }
        if found == 0 && has_tables(&conn)? {
            return Err(CoreError::Migration {
                from: 0,
                to: supported,
                msg: "file has tables but no schema version; not a Tugboat database".into(),
            });
        }
        if found > 0 && found < supported {
            backup(&conn, data_dir, found, now_ms)?;
        }
        set_pragmas(&conn)?;
        run_migrations(&mut conn, migrations, found)?;
        if defaults {
            crate::folders::ensure_defaults(&conn)?;
        }
        Ok(Db { conn, path: Some(path) })
    }

    /// Fresh in-memory database at SCHEMA_VERSION (tests).
    pub fn open_in_memory() -> Result<Db> {
        let mut conn = Connection::open_in_memory()?;
        run_migrations(&mut conn, MIGRATIONS, 0)?;
        crate::folders::ensure_defaults(&conn)?;
        Ok(Db { conn, path: None })
    }

    /// Current `PRAGMA user_version`.
    pub fn user_version(&self) -> Result<u32> {
        read_version(&self.conn)
    }

    /// Start an IMMEDIATE transaction (writes take the lock up front, so two writers never
    /// deadlock on upgrade).
    pub fn tx(&mut self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
    }
}

/// Path of the backup written before migrating from `version` (rule 4).
pub fn backup_path(data_dir: &Path, version: u32) -> PathBuf {
    data_dir.join(format!("{DB_FILE}.bak-{version}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "CREATE TABLE a (x INTEGER); INSERT INTO a VALUES (1);";
    const V2: &str = "ALTER TABLE a ADD COLUMN y INTEGER DEFAULT 7;";
    const V2_BAD: &str = "ALTER TABLE a ADD COLUMN y INTEGER DEFAULT 7; ALTER TABLE nope ADD COLUMN z INTEGER;";

    // L24: new db at SCHEMA_VERSION with defaults.
    #[test]
    fn new_db_is_current_with_default_folders() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        assert_eq!(db.user_version().unwrap(), SCHEMA_VERSION);
        let n: i64 = db.conn.query_row("SELECT count(*) FROM folder", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
        // A new db is not backed up.
        assert!(!backup_path(dir.path(), 0).exists());
        let mem = Db::open_in_memory().unwrap();
        assert_eq!(mem.user_version().unwrap(), SCHEMA_VERSION);
        assert!(mem.path.is_none());
    }

    #[test]
    fn reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        drop(Db::open(dir.path()).unwrap());
        let db = Db::open(dir.path()).unwrap();
        let n: i64 = db.conn.query_row("SELECT count(*) FROM folder", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
    }

    // L24: a newer db is refused and the file is not touched.
    #[test]
    fn newer_db_refused_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE t (x); PRAGMA user_version = 99;").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        match Db::open(dir.path()) {
            Err(CoreError::NewerDatabase { found: 99, supported: 2 }) => {}
            other => panic!("expected NewerDatabase, got {:?}", other.map(|_| ())),
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // No backup and no WAL side files either.
        assert!(!backup_path(dir.path(), 99).exists());
        assert!(!dir.path().join("tugboat.db-wal").exists());
    }

    // L24: version 0 with tables is refused.
    #[test]
    fn version_zero_with_tables_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        Connection::open(&path).unwrap().execute_batch("CREATE TABLE stranger (x);").unwrap();
        match Db::open(dir.path()) {
            Err(CoreError::Migration { from: 0, .. }) => {}
            other => panic!("expected Migration error, got {:?}", other.map(|_| ())),
        }
    }

    // L24: upgrade writes .bak-<v> with the OLD content, then migrates step by step.
    #[test]
    fn upgrade_writes_backup_then_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let _ = std::fs::create_dir_all(dir.path());
        // Build a v1 db with the synthetic list's first step.
        {
            let mut d = Db::open_with_no_defaults(dir.path(), &[V1]).unwrap();
            assert_eq!(d.user_version().unwrap(), 1);
            d.tx().unwrap().commit().unwrap();
        }
        let d = Db::open_with_no_defaults(dir.path(), &[V1, V2]).unwrap();
        assert_eq!(d.user_version().unwrap(), 2);
        let y: i64 = d.conn.query_row("SELECT y FROM a", [], |r| r.get(0)).unwrap();
        assert_eq!(y, 7);
        // The backup is the v1 database: no column y.
        let bak = Connection::open(backup_path(dir.path(), 1)).unwrap();
        assert_eq!(read_version(&bak).unwrap(), 1);
        assert!(bak.prepare("SELECT y FROM a").is_err());
        let x: i64 = bak.query_row("SELECT x FROM a", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 1);
    }

    #[test]
    fn existing_backup_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        drop(Db::open_with_no_defaults(dir.path(), &[V1]).unwrap());
        std::fs::write(backup_path(dir.path(), 1), b"older backup").unwrap();
        drop(Db::open_with_no_defaults(dir.path(), &[V1, V2]).unwrap());
        assert_eq!(std::fs::read(backup_path(dir.path(), 1)).unwrap(), b"older backup");
        let extra = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("tugboat.db.bak-1-"))
            .count();
        assert_eq!(extra, 1);
    }

    // L24: a failing step rolls back and the db stays at the last good version.
    #[test]
    fn failing_step_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        drop(Db::open_with_no_defaults(dir.path(), &[V1]).unwrap());
        match Db::open_with_no_defaults(dir.path(), &[V1, V2_BAD]) {
            Err(CoreError::Migration { from: 1, to: 2, .. }) => {}
            other => panic!("expected Migration 1->2, got {:?}", other.map(|_| ())),
        }
        // Still version 1, column y not added (the whole step rolled back).
        let c = Connection::open(dir.path().join(DB_FILE)).unwrap();
        assert_eq!(read_version(&c).unwrap(), 1);
        assert!(c.prepare("SELECT y FROM a").is_err());
    }

    // v1 -> v2 on a real 0.1.x file: rows identical, 'file' insertable, indexes and the file_ref
    // column there, the backup is the v1 file, reopening is idempotent.
    #[test]
    fn v1_to_v2_keeps_every_row_and_accepts_files() {
        let dir = tempfile::tempdir().unwrap();
        let cols = "id, captured_at, origin_device, lat, lon, accuracy_m, name, name_lamport, name_dev, blob_hash, size, media_kind, ext, thumb_hash, thumb_size, content_lamport, content_dev, folder_id, folder_entered_at, placement_lamport, placement_dev, pinned, pinned_lamport, pinned_dev, deleted_at, deleted_lamport, deleted_dev, local_file_state, local_state_at";
        let before: Vec<String>;
        {
            let d = Db::open_with(dir.path(), &MIGRATIONS[..1]).unwrap();
            assert_eq!(d.user_version().unwrap(), 1);
            for n in 1..=25u8 {
                let kind = if n % 2 == 0 { "video" } else { "image" };
                d.conn
                    .execute(
                        "INSERT INTO item (id, captured_at, origin_device, name, name_lamport, name_dev, blob_hash, size, media_kind, ext, content_lamport, content_dev, folder_id, folder_entered_at, placement_lamport, placement_dev, pinned, pinned_lamport, pinned_dev, deleted_lamport, deleted_dev, local_file_state, local_state_at, thumb_hash, thumb_size, lat, lon)
                         VALUES (?1, ?2, zeroblob(32), ?3, 1, zeroblob(32), zeroblob(32), ?2, ?4, 'x', 1, zeroblob(32), zeroblob(16), 5, 1, zeroblob(32), 0, 1, zeroblob(32), 1, zeroblob(32), 'present', 9, NULL, NULL, 1.5, NULL)",
                        rusqlite::params![{ let mut id = vec![0u8; 15]; id.push(n); id }, 100 + n as i64, format!("name {n}"), kind],
                    )
                    .unwrap();
            }
            d.conn.execute("INSERT INTO file_ref (item_id, path) VALUES (x'00000000000000000000000000000001', 'C:/lib/a.jpg')", []).unwrap();
            before = dump(&d.conn, &format!("SELECT {cols} FROM item ORDER BY id"));
        }
        let d = Db::open(dir.path()).unwrap();
        assert_eq!(d.user_version().unwrap(), 2);
        assert_eq!(dump(&d.conn, &format!("SELECT {cols} FROM item ORDER BY id")), before, "every item row is identical after the rebuild");
        // The backup is the v1 file: a 'file' row is refused there, accepted here.
        let bak = Connection::open(backup_path(dir.path(), 1)).unwrap();
        assert_eq!(read_version(&bak).unwrap(), 1);
        let insert = "INSERT INTO item (id, captured_at, origin_device, name, name_lamport, name_dev, blob_hash, size, media_kind, ext, content_lamport, content_dev, folder_id, folder_entered_at, placement_lamport, placement_dev, pinned, pinned_lamport, pinned_dev, deleted_lamport, deleted_dev, local_file_state, local_state_at) VALUES (zeroblob(16), 1, zeroblob(32), 'n', 1, zeroblob(32), zeroblob(32), 1, 'file', '', 1, zeroblob(32), zeroblob(16), 1, 1, zeroblob(32), 0, 1, zeroblob(32), 1, zeroblob(32), 'present', 1)";
        assert!(bak.execute(insert, []).is_err(), "v1 does not know 'file'");
        assert_eq!(d.conn.execute(insert, []).unwrap(), 1, "v2 accepts a file");
        assert!(d.conn.execute(&insert.replace("'file'", "'audio'"), []).is_err(), "still a closed list");
        // The five indexes by name, and the new file_ref column.
        for ix in ["item_folder", "item_captured", "item_hash", "item_local", "item_entered"] {
            let n: i64 = d.conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1", [ix], |r| r.get(0)).unwrap();
            assert_eq!(n, 1, "index {ix}");
        }
        d.conn.execute("UPDATE file_ref SET mtime_ms = 5", []).unwrap();
        drop(d);
        // Reopening is idempotent and keeps the rows.
        let again = Db::open(dir.path()).unwrap();
        let n: i64 = again.conn.query_row("SELECT count(*) FROM item", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 26);
    }

    fn dump(c: &Connection, sql: &str) -> Vec<String> {
        let mut st = c.prepare(sql).unwrap();
        let n = st.column_count();
        st.query_map([], |r| Ok((0..n).map(|i| format!("{:?}", r.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"))).unwrap().map(|r| r.unwrap()).collect()
    }

    // A backup that cannot be written (no space, no rights) fails the upgrade BEFORE anything changes:
    // the file stays at v1, untouched, and the error is the visible Migration error.
    #[test]
    fn a_failed_backup_leaves_the_database_at_v1_untouched() {
        let dir = tempfile::tempdir().unwrap();
        drop(Db::open_with(dir.path(), &MIGRATIONS[..1]).unwrap());
        // The first backup name is taken by a file, the timestamped one by a directory: VACUUM INTO cannot write either.
        std::fs::write(backup_path(dir.path(), 1), b"older backup").unwrap();
        std::fs::create_dir(dir.path().join("tugboat.db.bak-1-777")).unwrap();
        match Db::open_inner(dir.path(), MIGRATIONS, true, 777) {
            Err(CoreError::Migration { from: 1, to: 2, msg }) => assert!(msg.contains("backup failed"), "{msg}"),
            other => panic!("expected a Migration error, got {:?}", other.map(|_| ())),
        }
        let c = Connection::open(dir.path().join(DB_FILE)).unwrap();
        assert_eq!(read_version(&c).unwrap(), 1, "still v1");
        assert!(c.prepare("SELECT mtime_ms FROM file_ref").is_err(), "no step ran");
        assert_eq!(std::fs::read(backup_path(dir.path(), 1)).unwrap(), b"older backup");
    }

    impl Db {
        /// Test helper: `open_with` minus ensure_defaults (synthetic schemas have no folder table).
        fn open_with_no_defaults(data_dir: &Path, migrations: &[&str]) -> Result<Db> {
            Db::open_inner(data_dir, migrations, false, crate::clock::SystemClock.now_ms())
        }
    }
}
