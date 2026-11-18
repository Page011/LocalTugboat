package app.localtugboat

import android.content.Context
import androidx.core.content.edit
import java.io.File

/**
 * Pure part of the heartbeat: finding "Tugboat was stopped" gaps in the beat log.
 *
 * The service appends one epoch-ms line every [BEAT_MS]. A deliberate stop (setting turned off,
 * unpaired) appends "S<ms>" so the quiet time after it is not reported as a failure. A gap is a
 * beat followed by nothing for more than [GAP_MS], whether the next beat exists yet or not.
 */
object HeartbeatLog {
    const val BEAT_MS = 30_000L
    const val GAP_MS = 120_000L
    private const val STOP = 'S'

    class Gap(val fromMs: Long, val toMs: Long)

    private class Entry(val ms: Long, val stop: Boolean)

    private fun parse(lines: List<String>): List<Entry> = lines.mapNotNull {
        val t = it.trim()
        // A torn last line from a killed process parses to null and is skipped.
        if (t.startsWith(STOP)) t.drop(1).toLongOrNull()?.let { ms -> Entry(ms, true) } else t.toLongOrNull()?.let { ms -> Entry(ms, false) }
    }

    /** The newest gap that began after [ackedMs] (the time the user dismissed the banner), or null. */
    fun lastGap(lines: List<String>, nowMs: Long, ackedMs: Long): Gap? {
        val e = parse(lines)
        var found: Gap? = null
        for (i in 1 until e.size) {
            if (!e[i - 1].stop && e[i].ms - e[i - 1].ms > GAP_MS) found = Gap(e[i - 1].ms, e[i].ms)
        }
        val last = e.lastOrNull()
        if (last != null && !last.stop && nowMs - last.ms > GAP_MS) found = Gap(last.ms, nowMs)
        return found?.takeIf { it.fromMs > ackedMs }
    }

    fun beatLine(nowMs: Long) = "$nowMs"
    fun stopLine(nowMs: Long) = "$STOP$nowMs"
}

/** File side of the heartbeat: filesDir/heartbeat.log, trimmed to the newest 1000 lines past 64 KB. */
object Heartbeat {
    private const val MAX_BYTES = 64 * 1024
    private const val PREF_ACK = "gap_acked_ms"

    private fun file(c: Context) = File(c.applicationContext.filesDir, "heartbeat.log")

    @Synchronized
    private fun append(c: Context, line: String) {
        try {
            val f = file(c)
            f.appendText(line + "\n")
            if (f.length() > MAX_BYTES) f.writeText(f.readLines().takeLast(1000).joinToString("\n") + "\n")
        } catch (e: Exception) {
            // A heartbeat that cannot be written would later show as a false "stopped" gap: say why.
            KLog.log(c, "ERROR heartbeat write failed: $e")
        }
    }

    fun beat(c: Context) = append(c, HeartbeatLog.beatLine(System.currentTimeMillis()))
    fun markStopped(c: Context) = append(c, HeartbeatLog.stopLine(System.currentTimeMillis()))

    @Synchronized
    fun lastGap(c: Context, nowMs: Long): HeartbeatLog.Gap? {
        val f = file(c)
        if (!f.exists()) return null
        val acked = Prefs.of(c).getLong(PREF_ACK, 0L)
        return HeartbeatLog.lastGap(f.readLines(), nowMs, acked)
    }

    /** The user saw the banner (or tapped Fix): do not show gaps that started before now. */
    fun acknowledge(c: Context) = Prefs.of(c).edit { putLong(PREF_ACK, System.currentTimeMillis()) }
}
