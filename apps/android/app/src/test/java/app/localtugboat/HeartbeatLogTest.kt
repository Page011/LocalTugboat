package app.localtugboat

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Test

class HeartbeatLogTest {
    private val beat = HeartbeatLog.BEAT_MS
    private val gap = HeartbeatLog.GAP_MS

    private fun beats(from: Long, count: Int) = (0 until count).map { HeartbeatLog.beatLine(from + it * beat) }

    @Test fun steady_beats_are_no_gap() {
        val lines = beats(1_000, 20)
        assertNull(HeartbeatLog.lastGap(lines, 1_000 + 19 * beat + 5_000, 0))
    }

    @Test fun empty_log_is_no_gap() {
        assertNull(HeartbeatLog.lastGap(emptyList(), 999_999_999, 0))
    }

    @Test fun a_hole_between_beats_is_a_gap_from_the_last_beat() {
        val a = beats(1_000, 3)
        val restart = 1_000 + 2 * beat + gap + 1
        val g = HeartbeatLog.lastGap(a + HeartbeatLog.beatLine(restart), restart + 1_000, 0)
        assertNotNull(g)
        assertEquals(1_000 + 2 * beat, g!!.fromMs)
        assertEquals(restart, g.toMs)
    }

    @Test fun exactly_the_threshold_is_not_a_gap() {
        val lines = listOf(HeartbeatLog.beatLine(1_000), HeartbeatLog.beatLine(1_000 + gap))
        assertNull(HeartbeatLog.lastGap(lines, 1_000 + gap, 0))
    }

    @Test fun a_silent_tail_is_a_running_gap_until_now() {
        val lines = beats(1_000, 3)
        val last = 1_000 + 2 * beat
        val now = last + gap + 1
        val g = HeartbeatLog.lastGap(lines, now, 0)
        assertEquals(last, g!!.fromMs)
        assertEquals(now, g.toMs)
        assertNull(HeartbeatLog.lastGap(lines, last + gap, 0))
    }

    @Test fun a_deliberate_stop_is_never_a_gap() {
        val lines = beats(1_000, 3) + HeartbeatLog.stopLine(1_000 + 2 * beat + 100)
        assertNull(HeartbeatLog.lastGap(lines, 10_000_000, 0)) // quiet for hours after the stop
        // The service starts again hours later: the quiet time after the stop marker is still no gap.
        val later = lines + beats(9_000_000, 2)
        assertNull(HeartbeatLog.lastGap(later, 9_000_000 + beat + 1, 0))
    }

    @Test fun a_stop_marker_does_not_hide_a_later_real_gap() {
        val lines = beats(1_000, 2) + HeartbeatLog.stopLine(50_000) + beats(1_000_000, 2)
        val real = 1_000_000 + beat + gap + 10
        val g = HeartbeatLog.lastGap(lines, real, 0)
        assertEquals(1_000_000 + beat, g!!.fromMs)
    }

    @Test fun dismissing_hides_the_gap_but_not_a_newer_one() {
        val a = beats(1_000, 2)
        val firstGapStart = 1_000 + beat
        val restart = firstGapStart + 10 * gap
        val lines = a + beats(restart, 2)
        val acked = restart + 5_000
        assertNull(HeartbeatLog.lastGap(lines, restart + beat + 1, acked))
        // A newer gap after the dismissal shows again, and it is the newest one that is returned.
        val second = lines + HeartbeatLog.beatLine(restart + beat + 3 * gap)
        val g = HeartbeatLog.lastGap(second, restart + beat + 3 * gap + 1, acked)
        assertEquals(restart + beat, g!!.fromMs)
    }

    @Test fun a_gap_still_running_when_dismissed_stays_hidden_after_the_restart() {
        val lines = beats(1_000, 2)
        val now = 1_000 + beat + 3 * gap
        assertNotNull(HeartbeatLog.lastGap(lines, now, 0))
        val acked = now
        assertNull(HeartbeatLog.lastGap(lines, now + gap, acked))
        val after = lines + beats(now + gap, 2)
        assertNull(HeartbeatLog.lastGap(after, now + gap + beat, acked))
    }

    @Test fun torn_and_junk_lines_are_skipped() {
        val lines = listOf("", "abc", "12", HeartbeatLog.beatLine(1_000), "S", "17000000x")
        assertNull(HeartbeatLog.lastGap(lines, 1_000 + beat, 0))
        assertFalse(HeartbeatLog.lastGap(lines, 1_000 + gap + 1, 0) == null)
    }

    @Test fun wrap_format_round_trips_and_rejects_damage() {
        val iv = ByteArray(12) { it.toByte() }
        val ct = ByteArray(48) { (100 + it).toByte() }
        val (iv2, ct2) = WrapFormat.decode(WrapFormat.encode(iv, ct))
        assertArrayEquals(iv, iv2)
        assertArrayEquals(ct, ct2)
        assertThrows(IllegalArgumentException::class.java) { WrapFormat.decode(ByteArray(0)) }
        assertThrows(IllegalArgumentException::class.java) { WrapFormat.decode(byteArrayOf(12, 1, 2, 3)) } // IV cut short
        assertThrows(IllegalArgumentException::class.java) { WrapFormat.decode(WrapFormat.encode(iv, ct).copyOf(13)) } // no ciphertext
        assertThrows(IllegalArgumentException::class.java) { WrapFormat.encode(ByteArray(0), ct) }
    }
}
