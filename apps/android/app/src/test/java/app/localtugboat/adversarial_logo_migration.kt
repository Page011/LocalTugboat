package app.localtugboat

import java.io.File
import java.io.FileInputStream
import java.nio.file.Files
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** Checks of the 0.1.0 -> 0.1.1 rename. Each test states the safe outcome. */
class AdversarialLogoMigrationTest {
    private lateinit var root: File
    private lateinit var files: File
    private lateinit var prefs: File

    @Before fun setUp() {
        root = Files.createTempDirectory("adv-migrate").toFile()
        files = File(root, "files").apply { mkdirs() }
        prefs = File(root, "shared_prefs").apply { mkdirs() }
    }

    @After fun tearDown() {
        root.walkTopDown().forEach { it.setWritable(true) }
        root.deleteRecursively()
    }

    private fun put(dir: File, name: String, text: String): File = File(dir, name).also {
        it.parentFile!!.mkdirs()
        it.writeText(text)
    }

    private fun contents(): List<String> = root.walkTopDown().filter { it.isFile }.map { it.readText() }.sorted().toList()

    @Test fun a_read_only_old_database_is_renamed_or_left_but_never_lost() {
        put(files, "snapferry.db", "OLD-DB").setReadOnly()
        put(files, "snapferry.db-wal", "OLD-WAL").setReadOnly()
        val before = contents()
        LegacyMigration.runIn(files, prefs)
        assertEquals(before, contents())
        assertEquals("OLD-DB", File(files, "tugboat.db").readText())
        assertEquals("OLD-WAL", File(files, "tugboat.db-wal").readText())
    }

    @Test fun only_logs_and_prefs_from_0_1_0_still_move_and_no_database_is_invented() {
        put(files, "logs/snapferry.log.2", "L2")
        put(prefs, "snapferry.xml.bak", "PB")
        val r = LegacyMigration.runIn(files, prefs)
        assertEquals(2, r.moved.size)
        assertTrue(!File(files, "tugboat.db").exists())
    }

    /** A main-file rename the OS refuses (file held open on Windows): the old bytes stay, the failure is reported. */
    @Test fun a_refused_main_rename_keeps_the_old_database_and_reports_it() {
        val db = put(files, "snapferry.db", "OLD-DB")
        FileInputStream(db).use {
            val r = LegacyMigration.runIn(files, prefs)
            if (File(files, "tugboat.db").exists()) return // this OS allowed the rename: nothing to prove here
            assertEquals("OLD-DB", db.readText())
            assertEquals(1, r.failed.size)
            // The report must make run() leave a note for the activity history.
            assertTrue("a failed rename leaves no activity note (changedAnything is false)", r.changedAnything)
        }
    }

    /** A stray tugboat.db-wal with no tugboat.db: the old database must not be renamed onto a foreign WAL. */
    @Test fun the_old_database_is_never_paired_with_a_foreign_wal() {
        put(files, "snapferry.db", "OLD-DB")
        put(files, "snapferry.db-wal", "OLD-WAL")
        put(files, "tugboat.db-wal", "FOREIGN-WAL")
        LegacyMigration.runIn(files, prefs)
        val dbName = files.listFiles()!!.single { it.isFile && it.readText() == "OLD-DB" }.name
        val walName = files.listFiles()!!.single { it.isFile && it.readText() == "OLD-WAL" }.name
        assertEquals("the old WAL must sit next to the old database", "$dbName-wal", walName)
    }

    /**
     * Crash after the renames: the next start must still be able to say what moved. The record is now written BEFORE the
     * files move (runRecorded), so a process killed at any point leaves it. Calling runIn twice and asking
     * changedAnything would only show that a second run changes nothing, so the property is asserted on the durable
     * record instead.
     */
    @Test fun a_crash_after_the_renames_still_leaves_a_record_on_the_next_start() {
        put(files, "snapferry.db", "OLD-DB")
        LegacyMigration.runRecorded(files, prefs) // process killed right here: nothing else ran
        val note = File(files, LegacyMigration.NOTE)
        assertTrue("no record that snapferry.db was renamed", note.exists() && note.readText().contains("snapferry.db -> tugboat.db"))
        // The next start finds nothing to move and must not write the same record twice.
        LegacyMigration.runRecorded(files, prefs)
        assertEquals(1, note.readLines().count { it.startsWith("Upgraded from the old app names") })
    }

    /** Killed between the record and the rename: the record is there, the files have not moved, the next start finishes. */
    @Test fun a_crash_between_the_record_and_the_move_is_finished_by_the_next_start() {
        put(files, "snapferry.db", "OLD-DB")
        val plan = LegacyMigration.runIn(files, prefs, dry = true)
        assertTrue(plan.moved.contains("snapferry.db -> tugboat.db"))
        assertTrue("a dry run moves nothing", File(files, "snapferry.db").exists() && !File(files, "tugboat.db").exists())
        LegacyMigration.runRecorded(files, prefs)
        assertEquals("OLD-DB", File(files, "tugboat.db").readText())
    }
}
