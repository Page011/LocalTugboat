package app.localtugboat

import java.time.ZoneId
import org.junit.Assert.assertEquals
import org.junit.Test
import uniffi.tugboat_ffi.ItemStateInfo
import uniffi.tugboat_ffi.SnapException
import uniffi.tugboat_ffi.SyncState

class TextsTest {
    private val min = 60_000L
    private val hour = 60 * min
    private val day = 24 * hour

    private fun status(state: SyncState, last: Long? = null, waiting: UInt = 0u, message: String? = null) =
        TestData.status(state, waiting = waiting, message = message, lastSyncedAt = last)

    @Test fun delete_forever_questions_say_what_goes_and_from_where() {
        assertEquals("Delete forever?", Texts.DELETE_FOREVER_TITLE)
        assertEquals("Removes it from every linked device.", Texts.DELETE_FOREVER_TEXT)
        assertEquals("Empty Bin?", Texts.EMPTY_BIN_TITLE)
        assertEquals("Removes 12 items from every linked device.", Texts.emptyBinText(12))
        assertEquals("Removes 2 items from every linked device.", Texts.emptyBinText(2))
        assertEquals("Removes 1 item from every linked device.", Texts.emptyBinText(1))
    }

    @Test fun duration_rounds_down_and_switches_unit() {
        assertEquals("<1m", Texts.duration(59_999))
        assertEquals("1m", Texts.duration(min))
        assertEquals("59m", Texts.duration(59 * min + 59_000))
        assertEquals("1h", Texts.duration(hour))
        assertEquals("5h", Texts.duration(5 * hour + 40 * min))
        assertEquals("47h", Texts.duration(47 * hour))
        assertEquals("2d", Texts.duration(2 * day))
    }

    @Test fun ago_is_just_now_then_seconds_then_coarser() {
        assertEquals("just now", Texts.ago(0))
        assertEquals("just now", Texts.ago(-30_000)) // clock moved back: never "-30s ago"
        assertEquals("10s ago", Texts.ago(10_000))
        assertEquals("3m ago", Texts.ago(3 * min))
        assertEquals("5h ago", Texts.ago(5 * hour))
    }

    @Test fun expiry_labels() {
        val now = 1_000_000_000L
        assertEquals("Expires in 5h", Texts.expiryLabel(ItemStateInfo.Live(now + 5 * hour + 10 * min), now, 7u))
        assertEquals("Kept", Texts.expiryLabel(ItemStateInfo.Kept, now, 7u))
        assertEquals("Kept (folder)", Texts.expiryLabel(ItemStateInfo.FolderKeep, now, 7u))
    }

    @Test fun bin_label_counts_days_left_rounding_up() {
        val now = 100 * day
        assertEquals("In Bin, 7d left", Texts.expiryLabel(ItemStateInfo.Deleted(now), now, 7u))
        assertEquals("In Bin, 7d left", Texts.expiryLabel(ItemStateInfo.Expired(now - hour), now, 7u)) // 6d23h left rounds up
        assertEquals("In Bin, 1d left", Texts.expiryLabel(ItemStateInfo.Deleted(now - 6 * day - hour), now, 7u))
        assertEquals("In Bin, removing soon", Texts.expiryLabel(ItemStateInfo.Deleted(now - 7 * day), now, 7u))
        assertEquals("In Bin, removing soon", Texts.expiryLabel(ItemStateInfo.Deleted(now - 30 * day), now, 7u))
    }

    @Test fun camera_sync_text() {
        val now = 10 * hour
        assertEquals("Synced 10s ago", Texts.cameraSync(status(SyncState.SYNCED, last = now - 10_000), now))
        assertEquals("PC not found, 3 waiting", Texts.cameraSync(status(SyncState.OFFLINE, waiting = 3u), now))
    }

    @Test fun stopped_banner_uses_local_time() {
        // 2026-11-17 14:02:30 UTC shown in UTC and in Sydney (UTC+11 in November).
        val ms = java.time.ZonedDateTime.of(2026, 11, 17, 14, 2, 30, 0, ZoneId.of("UTC")).toInstant().toEpochMilli()
        assertEquals("STOPPED at 14:02 - Tugboat was not running", Texts.stoppedAt(ms, ZoneId.of("UTC")))
        assertEquals("STOPPED at 01:02 - Tugboat was not running", Texts.stoppedAt(ms, ZoneId.of("Australia/Sydney")))
    }

    @Test fun pair_failures_name_the_cause() {
        assertEquals("That is not a Tugboat code", Texts.pairFailure(SnapException.PairBadPayload("x"), true))
        assertEquals("PC NOT FOUND - same Wi-Fi as the PC?", Texts.pairFailure(SnapException.NetTimeout("t"), true))
        assertEquals("LOCAL NETWORK BLOCKED - allow it, then scan again", Texts.pairFailure(SnapException.NetUnreachable("u"), false))
        assertEquals("PAIRING CODE EXPIRED - show a new QR", Texts.pairFailure(SnapException.PairExpired("PAIRING CODE EXPIRED - show a new QR"), true))
    }

    @Test fun an_old_qr_never_reads_as_not_a_tugboat_code() {
        // An expired QR never says "That is not a Tugboat code": only a payload the core cannot parse does.
        assertEquals("PAIRING CODE EXPIRED - show a new QR", Texts.pairFailure(SnapException.PairExpired("PAIRING CODE EXPIRED - show a new QR"), false))
        assertEquals("Pairing is not open on the PC", Texts.pairFailure(SnapException.PairNotOpen("Pairing is not open on the PC"), true))
        assertEquals("Update Tugboat on the PC", Texts.pairFailure(SnapException.PairVersionMismatch("Update Tugboat on the PC"), true))
        assertEquals("That is not a Tugboat code", Texts.pairFailure(SnapException.PairBadPayload("bad"), true))
    }

    @Test fun an_old_qr_refused_by_the_pcs_newer_window_reads_expired() {
        // After 2 minutes the PC opens a new window, so the old QR comes back as a wrong secret.
        assertEquals("WRONG PAIRING CODE - scan the QR again", Texts.pairFailure(SnapException.PairBadSecret("WRONG PAIRING CODE - scan the QR again"), true))
        assertEquals("PAIRING CODE EXPIRED - show a new QR", Texts.pairFailure(SnapException.PairBadSecret("WRONG PAIRING CODE - scan the QR again"), true, codeExpired = true))
        assertEquals("PAIRING CODE EXPIRED - show a new QR", Texts.pairFailure(SnapException.PairNotOpen("Pairing is not open on the PC"), true, codeExpired = true))
        // A refusal that has nothing to do with the code's age keeps its words.
        assertEquals("Update Tugboat on the PC", Texts.pairFailure(SnapException.PairVersionMismatch("Update Tugboat on the PC"), true, codeExpired = true))
    }

    @Test fun storage_low_text_counts_what_waits_on_the_pc() {
        assertEquals("PHONE STORAGE LOW - 37 items wait on the PC", Texts.storageLow(37u))
        assertEquals("PHONE STORAGE LOW - 2 items wait on the PC", Texts.storageLow(2u))
        assertEquals("PHONE STORAGE LOW - 1 item waits on the PC", Texts.storageLow(1u))
        assertEquals("PHONE STORAGE LOW", Texts.storageLow(0u))
    }

    @Test fun expiry_labels_have_no_waiting_state() {
        val now = 100 * day
        for (st in listOf(ItemStateInfo.Kept, ItemStateInfo.FolderKeep, ItemStateInfo.Live(now + hour), ItemStateInfo.Deleted(now), ItemStateInfo.Expired(now))) {
            assertEquals(false, Texts.expiryLabel(st, now, 7u).contains("aiting"))
        }
    }
}
