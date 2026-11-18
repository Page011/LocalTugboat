package app.localtugboat

import android.content.Context
import java.io.File

/** Thrown by [Core.phone] while the 0.1.0 library is still under its old name. The message is the blocker text. */
class UpgradeStoppedException(message: String) : Exception(message)

/**
 * Upgrade from 0.1.0 (the build that used the "snapferry" file names, same applicationId) to 0.1.1.
 *
 * The rename run changed three file names in the app's private storage, and this phone already holds
 * photos under the old ones. Without this step the new build would open an EMPTY tugboat.db next to the
 * old snapferry.db and every held photo would be orphaned. Rules:
 *
 *  - a file is renamed only when the old name exists and the new name does not (never overwrites);
 *  - the database and its sidecars move as one group; if BOTH main databases exist, or a new-name sidecar
 *    (tugboat.db-wal / -shm / -journal) is already there, nothing in that group is touched (a conflict a person
 *    must look at). A group that did not move, or whose rename failed, STOPS the upgrade: the core is not
 *    opened (it would create an empty tugboat.db and the photos would look gone) and the screen shows
 *    "UPGRADE STOPPED" with an Export log button. Nothing is deleted either way;
 *  - rename, never copy-and-delete, never delete: after a run every byte is still on disk under one name;
 *  - safe to run on every start and after a crash half-way (each file is judged on its own, sidecars follow
 *    a main database that already moved);
 *  - runs BEFORE the Rust core opens anything, so the core only ever sees the new names.
 *
 * What is NOT renamed: identity.key (same name in 0.1.0; its Keystore alias changed, see [KeyVault]),
 * storage/items, storage/bin, storage/parts, thumbs/ and kotlin.log (names never changed).
 */
object LegacyMigration {
    const val OLD_DB = "snapferry.db"
    const val NEW_DB = "tugboat.db"
    const val OLD_LOG = "snapferry.log"
    const val NEW_LOG = "tugboat.log"
    const val OLD_PREFS = "snapferry"
    const val NEW_PREFS = "tugboat"

    /** SQLite sidecars of the main file; the core's own backups are named "<db>.bak-<version>-...". */
    private val SIDECARS = listOf("-wal", "-shm", "-journal")

    /** Written next to the data when something moved; the activity history entry is made once the core is open. */
    const val NOTE = "upgrade-note.txt"
    private const val HEAD = "Upgraded from the old app names"
    const val NOTE_LOGGED = "upgrade-note.logged"
    private const val STOP_HEAD = "UPGRADE STOPPED - old library could not be moved"

    /** Set by [run] when the database group could not go over: [Core.phone] then refuses to open. Cleared by a good run. */
    @Volatile var stopMessage: String? = null
        private set

    class Report(
        /** "relative/old -> relative/new" for every rename done in this run. */
        val moved: List<String>,
        /** Group or file left alone because the new name already exists, with the reason. */
        val skipped: List<String>,
        /** Renames that failed (the old file is still where it was). */
        val failed: List<String>,
        /** Why the main database group did not go over (conflict or refused rename). Empty = the core may open. */
        val stops: List<String> = emptyList(),
    ) {
        /** Something happened that the history must record: a rename, or a rename that failed. */
        val changedAnything get() = moved.isNotEmpty() || failed.isNotEmpty()

        /** True when the core must not open: the old library is still under its old name. */
        val blocksCore get() = stops.isNotEmpty()

        /** The blocker text for the screen: one short line. Null when the core may open. */
        val stopMessage: String? get() = if (blocksCore) "$STOP_HEAD (${stops.first()})" else null

        /** One short line for the activity history. */
        fun activityText(): String = buildString {
            if (moved.isNotEmpty() || stops.isEmpty()) {
                append("$HEAD: renamed ${moved.joinToString(", ") { it.substringBefore(" -> ") }} (nothing deleted)")
                if (skipped.isNotEmpty()) append(". LEFT ALONE: ${skipped.size} (both names exist)")
                if (failed.isNotEmpty()) append(". FAILED: ${failed.size}")
                if (stops.isNotEmpty()) append(". ")
            }
            if (stops.isNotEmpty()) append("$STOP_HEAD: ${stops.joinToString("; ")} (nothing deleted)")
        }
    }

    /** Runs the whole upgrade for an app. Call first thing in Application.onCreate. */
    fun run(c: Context): Report {
        val ctx = c.applicationContext
        val filesDir = ctx.filesDir
        val prefsDir = File(filesDir.parentFile, "shared_prefs")
        val report = runRecorded(filesDir, prefsDir) { KLog.log(ctx, it) }
        stopMessage = report.stopMessage
        for (line in report.moved) KLog.log(ctx, "upgrade: renamed $line")
        for (line in report.skipped) KLog.log(ctx, "upgrade: LEFT ALONE $line")
        for (line in report.failed) KLog.log(ctx, "ERROR upgrade: rename failed $line")
        if (report.blocksCore) KLog.log(ctx, "ERROR upgrade: core stays closed: ${report.stopMessage}")
        if (report.changedAnything || report.blocksCore) KLog.log(ctx, "upgrade: ${report.activityText()}")
        return report
    }

    /**
     * The record comes BEFORE the move: a dry run says what is about to be renamed, that goes into the note, and
     * only then do the files move. A process killed anywhere in between still leaves the note (the next start
     * plans the same moves, finds the same head line already in the note and does not repeat it).
     * A rename that then fails, or a stop, adds its own line. Pure on two directories: the JVM test calls it.
     */
    fun runRecorded(filesDir: File, prefsDir: File, log: (String) -> Unit = {}): Report {
        val plan = runIn(filesDir, prefsDir, dry = true)
        if (plan.moved.isNotEmpty() || plan.blocksCore) record(filesDir, plan, log)
        val done = runIn(filesDir, prefsDir)
        if (done.failed.isNotEmpty() || (done.blocksCore && !plan.blocksCore)) record(filesDir, done, log)
        return done
    }

    /** Appends the report to the note unless its head line is already in it (a stop that repeats every start is one row). */
    private fun record(filesDir: File, r: Report, log: (String) -> Unit) {
        try {
            val note = File(filesDir, NOTE)
            val head = r.activityText()
            if (note.exists() && note.readLines().any { it == head }) return
            filesDir.mkdirs()
            note.appendText(head + "\n" + r.moved.joinToString("\n") + (if (r.moved.isEmpty()) "" else "\n"))
        } catch (e: Exception) {
            log("ERROR upgrade: note not written: $e")
        }
    }

    /** The pure part: the same logic on any two directories, so the JVM test can run it on a temp folder. */
    fun runIn(filesDir: File, prefsDir: File, dry: Boolean = false): Report {
        val moved = mutableListOf<String>()
        val skipped = mutableListOf<String>()
        val failed = mutableListOf<String>()
        val stops = mutableListOf<String>()

        /** Returns true when the file is (or, in a dry run, would be) under the new name afterwards. */
        fun rename(dir: File, oldName: String, newName: String, label: String, main: Boolean = false): Boolean {
            val from = File(dir, oldName)
            val to = File(dir, newName)
            if (!from.exists()) return false
            if (to.exists()) {
                skipped += "$label$oldName (new name $newName exists)"
                if (main) stops += "$oldName not moved, $newName exists"
                return false
            }
            if (dry || from.renameTo(to)) {
                moved += "$label$oldName -> $label$newName"
                return true
            }
            failed += "$label$oldName -> $label$newName"
            if (main) stops += "$oldName rename refused"
            return false
        }

        // Database group.
        val oldDb = File(filesDir, OLD_DB)
        val newDb = File(filesDir, NEW_DB)
        val foreign = SIDECARS.filter { File(filesDir, NEW_DB + it).exists() }
        val dbWentOver = when {
            oldDb.exists() && newDb.exists() -> {
                skipped += "$OLD_DB and $NEW_DB both exist: database group not touched"
                stops += "$OLD_DB and $NEW_DB both exist"
                false
            }
            // The old database must never sit next to a sidecar that belongs to something else: SQLite would replay it.
            oldDb.exists() && foreign.isNotEmpty() -> {
                skipped += "$OLD_DB: ${foreign.joinToString { NEW_DB + it }} already exists, database group not touched"
                stops += "${foreign.joinToString { NEW_DB + it }} exists without $NEW_DB"
                false
            }
            oldDb.exists() -> rename(filesDir, OLD_DB, NEW_DB, "", main = true)
            // An earlier run moved the main file and was cut off before its sidecars.
            else -> newDb.exists()
        }
        if (dbWentOver) {
            for (s in SIDECARS) rename(filesDir, OLD_DB + s, NEW_DB + s, "")
            val backups = filesDir.listFiles { f -> f.isFile && f.name.startsWith("$OLD_DB.bak-") }.orEmpty().sortedBy { it.name }
            for (b in backups) rename(filesDir, b.name, NEW_DB + b.name.removePrefix(OLD_DB), "")
        }

        // Rust log and its two rotated files.
        val logs = File(filesDir, "logs")
        for (suffix in listOf("", ".1", ".2")) rename(logs, OLD_LOG + suffix, NEW_LOG + suffix, "logs/")

        // Small per-app flags (banner dismissals, one-time prompts).
        for (suffix in listOf(".xml", ".xml.bak")) rename(prefsDir, OLD_PREFS + suffix, NEW_PREFS + suffix, "shared_prefs/")

        return Report(moved, skipped, failed, stops)
    }

    /**
     * Once the core is open: put the note [run] left into the activity history, then keep the note under a
     * "logged" name so it is never logged twice. Blocking (core call): call from a background thread.
     */
    fun flushActivity(c: Context, logToActivity: (String) -> Unit) =
        flushIn(c.applicationContext.filesDir, { KLog.log(c, it) }, logToActivity)

    /** The pure part of [flushActivity]. Returns how many activity rows were written. */
    fun flushIn(dir: File, log: (String) -> Unit, logToActivity: (String) -> Unit): Int {
        val note = File(dir, NOTE)
        if (!note.exists()) return 0
        var written = 0
        try {
            // One activity row per run that moved something (the head line of each block).
            val lines = note.readLines()
            val heads = lines.filter { it.startsWith(HEAD) || it.startsWith(STOP_HEAD) }
            for (h in heads.ifEmpty { listOf(lines.firstOrNull().orEmpty()) }) {
                logToActivity(h)
                written++
            }
            // Renamed, not deleted: the record of what moved stays on the phone.
            if (!note.renameTo(File(dir, "$NOTE_LOGGED.${System.nanoTime()}.txt"))) log("ERROR upgrade: note not renamed")
        } catch (e: Exception) {
            log("ERROR upgrade: activity entry not written: $e")
        }
        return written
    }
}
