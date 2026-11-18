package app.localtugboat

import java.io.File
import java.nio.file.Files
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** The 0.1.0 -> 0.1.1 upgrade: no photo may be orphaned and nothing may be deleted. */
class LegacyMigrationTest {
    private lateinit var root: File
    private lateinit var files: File
    private lateinit var prefs: File

    @Before fun setUp() {
        root = Files.createTempDirectory("tugboat-migrate").toFile()
        files = File(root, "files").apply { mkdirs() }
        prefs = File(root, "shared_prefs").apply { mkdirs() }
    }

    @After fun tearDown() {
        root.deleteRecursively()
    }

    private fun put(dir: File, name: String, text: String): File = File(dir, name).also {
        it.parentFile!!.mkdirs()
        it.writeText(text)
    }

    private fun everyFile(): Map<String, String> =
        root.walkTopDown().filter { it.isFile }.associate { it.relativeTo(root).path.replace('\\', '/') to it.readText() }

    /** The 0.1.0 layout: old names, three photos under storage/ (names that never changed), the identity key. */
    private fun plantOldPhone() {
        put(files, "snapferry.db", "DB-BYTES")
        put(files, "snapferry.db-wal", "WAL-BYTES")
        put(files, "snapferry.db-shm", "SHM-BYTES")
        put(files, "snapferry.db.bak-1-20261117", "BACKUP-BYTES")
        put(files, "identity.key", "WRAPPED-KEY")
        put(files, "kotlin.log", "kotlin log")
        put(files, "logs/snapferry.log", "log0")
        put(files, "logs/snapferry.log.1", "log1")
        put(files, "storage/items/a.jpg", "PHOTO-A")
        put(files, "storage/items/b.jpg", "PHOTO-B")
        put(files, "storage/bin/c.mp4", "VIDEO-C")
        put(files, "thumbs/a.jpg", "THUMB-A")
        put(prefs, "snapferry.xml", "<map/>")
    }

    @Test fun old_names_become_new_names_and_nothing_is_lost() {
        plantOldPhone()
        val before = everyFile()
        val r = LegacyMigration.runIn(files, prefs)

        val after = everyFile()
        assertEquals("no file added or deleted", before.size, after.size)
        // Every old byte string is still there, under exactly the new name.
        val expected = mapOf(
            "files/snapferry.db" to "files/tugboat.db",
            "files/snapferry.db-wal" to "files/tugboat.db-wal",
            "files/snapferry.db-shm" to "files/tugboat.db-shm",
            "files/snapferry.db.bak-1-20261117" to "files/tugboat.db.bak-1-20261117",
            "files/logs/snapferry.log" to "files/logs/tugboat.log",
            "files/logs/snapferry.log.1" to "files/logs/tugboat.log.1",
            "shared_prefs/snapferry.xml" to "shared_prefs/tugboat.xml",
        )
        for ((old, new) in expected) {
            assertFalse("$old still there", after.containsKey(old))
            assertEquals("content of $new", before[old], after[new])
        }
        // Names that never changed are untouched, including every photo.
        for (keep in listOf("files/identity.key", "files/kotlin.log", "files/storage/items/a.jpg", "files/storage/items/b.jpg", "files/storage/bin/c.mp4", "files/thumbs/a.jpg")) {
            assertEquals(before[keep], after[keep])
        }
        assertEquals(expected.size, r.moved.size)
        assertTrue(r.skipped.isEmpty() && r.failed.isEmpty())
        assertTrue(r.changedAnything)
    }

    @Test fun a_second_run_changes_nothing() {
        plantOldPhone()
        LegacyMigration.runIn(files, prefs)
        val once = everyFile()
        val r = LegacyMigration.runIn(files, prefs)
        assertEquals(once, everyFile())
        assertFalse(r.changedAnything)
        assertTrue(r.skipped.isEmpty() && r.failed.isEmpty())
    }

    @Test fun a_fresh_install_is_left_alone() {
        put(files, "tugboat.db", "NEW")
        put(files, "identity.key", "K")
        val before = everyFile()
        val r = LegacyMigration.runIn(files, prefs)
        assertEquals(before, everyFile())
        assertFalse(r.changedAnything)
    }

    @Test fun both_databases_present_is_a_conflict_nothing_in_the_group_moves() {
        put(files, "snapferry.db", "OLD")
        put(files, "snapferry.db-wal", "OLD-WAL")
        put(files, "tugboat.db", "NEW")
        put(files, "logs/snapferry.log", "log0")
        val r = LegacyMigration.runIn(files, prefs)
        assertEquals("OLD", File(files, "snapferry.db").readText())
        assertEquals("NEW", File(files, "tugboat.db").readText())
        // The old WAL must never be attached to the other database.
        assertTrue(File(files, "snapferry.db-wal").exists())
        assertFalse(File(files, "tugboat.db-wal").exists())
        assertEquals(1, r.skipped.size)
        // Files outside the database group still follow their own rule.
        assertTrue(File(files, "logs/tugboat.log").exists())
        assertTrue(r.activityText().contains("renamed logs/snapferry.log"))
    }

    @Test fun nothing_is_overwritten_when_a_new_name_exists() {
        put(files, "logs/snapferry.log", "OLD-LOG")
        put(files, "logs/tugboat.log", "NEW-LOG")
        put(prefs, "snapferry.xml", "OLD-PREFS")
        put(prefs, "tugboat.xml", "NEW-PREFS")
        val before = everyFile()
        val r = LegacyMigration.runIn(files, prefs)
        assertEquals(before, everyFile())
        assertEquals(2, r.skipped.size)
        assertFalse(r.changedAnything)
    }

    @Test fun a_run_cut_off_after_the_main_database_moved_finishes_the_sidecars() {
        put(files, "tugboat.db", "DB")
        put(files, "snapferry.db-wal", "WAL-NOT-YET-MOVED")
        put(files, "snapferry.db-shm", "SHM")
        val r = LegacyMigration.runIn(files, prefs)
        assertEquals("WAL-NOT-YET-MOVED", File(files, "tugboat.db-wal").readText())
        assertFalse(File(files, "snapferry.db-wal").exists())
        assertEquals(2, r.moved.size)
    }

    @Test fun a_resumed_run_with_a_conflicting_wal_stops_the_core_and_changes_nothing() {
        put(files, "tugboat.db", "DB")
        put(files, "snapferry.db-wal", "OLD-WAL-WITH-COMMITTED-DATA")
        put(files, "tugboat.db-wal", "OTHER-WAL")
        val before = everyFile()
        val r = LegacyMigration.runIn(files, prefs)
        assertTrue(r.blocksCore)
        assertTrue(r.stopMessage!!.startsWith("UPGRADE STOPPED"))
        assertEquals(before, everyFile())
    }

    @Test fun a_wal_without_any_database_is_not_touched() {
        put(files, "snapferry.db-wal", "ORPHAN")
        val r = LegacyMigration.runIn(files, prefs)
        assertTrue(File(files, "snapferry.db-wal").exists())
        assertFalse(r.changedAnything)
    }

    @Test fun missing_folders_are_fine() {
        files.deleteRecursively()
        prefs.deleteRecursively()
        val r = LegacyMigration.runIn(files, prefs)
        assertFalse(r.changedAnything)
        assertTrue(r.failed.isEmpty())
    }

    @Test fun the_activity_text_names_every_renamed_file_and_says_nothing_was_deleted() {
        plantOldPhone()
        val t = LegacyMigration.runIn(files, prefs).activityText()
        assertTrue(t.startsWith("Upgraded from the old app names: renamed snapferry.db"))
        assertTrue(t.contains("shared_prefs/snapferry.xml"))
        assertTrue(t.endsWith("(nothing deleted)"))
        assertFalse("one line", t.contains('\n'))
        assertTrue("fits the core's 300 char limit", t.length <= 300)
    }

    @Test fun the_activity_note_is_logged_once_and_kept() {
        File(files, LegacyMigration.NOTE).writeText("Upgraded from the old app names: renamed snapferry.db (nothing deleted)\nsnapferry.db -> tugboat.db\n")
        val rows = mutableListOf<String>()
        val logs = mutableListOf<String>()
        assertEquals(1, LegacyMigration.flushIn(files, { logs += it }, { rows += it }))
        assertEquals(listOf("Upgraded from the old app names: renamed snapferry.db (nothing deleted)"), rows)
        assertFalse(File(files, LegacyMigration.NOTE).exists())
        assertEquals("note kept under a logged name", 1, files.listFiles { f -> f.name.startsWith(LegacyMigration.NOTE_LOGGED) }!!.size)
        // Second call: nothing to log.
        assertEquals(0, LegacyMigration.flushIn(files, { logs += it }, { rows += it }))
        assertEquals(1, rows.size)
        assertTrue(logs.isEmpty())
    }

    // ---- a group that cannot go over STOPS the upgrade (the screen shows a blocker, the core stays closed) ----

    @Test fun a_good_upgrade_does_not_block_the_core() {
        plantOldPhone()
        val r = LegacyMigration.runRecorded(files, prefs)
        assertFalse(r.blocksCore)
        assertEquals(null, r.stopMessage)
        assertEquals("PHOTO-A", File(files, "storage/items/a.jpg").readText())
        assertEquals("DB-BYTES", File(files, "tugboat.db").readText())
    }

    @Test fun both_databases_block_the_core_with_one_short_blocker_and_one_note() {
        put(files, "snapferry.db", "OLD")
        put(files, "tugboat.db", "NEW")
        val r1 = LegacyMigration.runRecorded(files, prefs)
        val r2 = LegacyMigration.runRecorded(files, prefs) // the same conflict on every start
        assertTrue(r1.blocksCore && r2.blocksCore)
        assertTrue(r1.stopMessage!!.startsWith("UPGRADE STOPPED - old library could not be moved"))
        assertFalse("one line", r1.stopMessage!!.contains('\n'))
        assertEquals("OLD", File(files, "snapferry.db").readText())
        assertEquals("NEW", File(files, "tugboat.db").readText())
        val note = File(files, LegacyMigration.NOTE).readLines().filter { it.startsWith("UPGRADE STOPPED") }
        assertEquals("a repeating stop is one history row, not one per start", 1, note.size)
        assertTrue(r1.activityText().length <= 300)
    }

    @Test fun a_foreign_wal_blocks_the_core_and_the_group_is_left_alone() {
        put(files, "snapferry.db", "OLD-DB")
        put(files, "snapferry.db-wal", "OLD-WAL")
        put(files, "tugboat.db-wal", "FOREIGN-WAL")
        val r = LegacyMigration.runRecorded(files, prefs)
        assertTrue(r.blocksCore)
        assertEquals("OLD-DB", File(files, "snapferry.db").readText())
        assertEquals("OLD-WAL", File(files, "snapferry.db-wal").readText())
        assertEquals("FOREIGN-WAL", File(files, "tugboat.db-wal").readText())
        assertFalse(File(files, "tugboat.db").exists())
    }

    @Test fun the_stop_text_reaches_the_activity_history_when_the_core_finally_opens() {
        put(files, "snapferry.db", "OLD")
        put(files, "tugboat.db", "NEW")
        LegacyMigration.runRecorded(files, prefs)
        val rows = mutableListOf<String>()
        assertEquals(1, LegacyMigration.flushIn(files, {}, { rows += it }))
        assertTrue(rows.single().startsWith("UPGRADE STOPPED - old library could not be moved"))
    }

    @Test fun the_record_is_written_before_the_files_move() {
        plantOldPhone()
        val plan = LegacyMigration.runIn(files, prefs, dry = true)
        assertTrue(File(files, "snapferry.db").exists() && !File(files, "tugboat.db").exists())
        val r = LegacyMigration.runRecorded(files, prefs)
        assertEquals(plan.moved, r.moved)
        val note = File(files, LegacyMigration.NOTE).readText()
        for (m in r.moved) assertTrue("note lists $m", note.contains(m))
    }

    // ---- the identity key's Keystore alias ----

    @Test fun the_current_alias_is_tried_first_then_the_0_1_0_one() {
        assertEquals(listOf("tugboat_identity_wrap", "snapferry_identity_wrap"), AliasOrder.of(setOf("snapferry_identity_wrap", "tugboat_identity_wrap", "other")))
        assertEquals(listOf("snapferry_identity_wrap"), AliasOrder.of(setOf("snapferry_identity_wrap")))
        assertEquals(emptyList<String>(), AliasOrder.of(setOf("other")))
    }

    @Test fun an_updated_phone_unwraps_its_old_key_with_the_old_alias() {
        // Both aliases exist but only the 0.1.0 one opens this file.
        val (alias, key) = AliasOrder.firstThatWorks(AliasOrder.of(setOf("tugboat_identity_wrap", "snapferry_identity_wrap"))) { a ->
            if (a == "snapferry_identity_wrap") "KEY" else throw IllegalStateException("tag mismatch")
        }
        assertEquals("snapferry_identity_wrap", alias)
        assertEquals("KEY", key)
    }

    @Test fun no_alias_that_opens_the_file_is_an_error_never_a_new_key() {
        var thrown: Exception? = null
        try {
            AliasOrder.firstThatWorks(listOf("tugboat_identity_wrap")) { throw IllegalStateException("bad tag") }
        } catch (e: Exception) {
            thrown = e
        }
        assertEquals("bad tag", thrown?.message)
        try {
            AliasOrder.firstThatWorks(emptyList<String>()) { it }
        } catch (e: Exception) {
            thrown = e
        }
        assertTrue(thrown is IllegalStateException && thrown.message!!.contains("no identity wrap key"))
    }
}
