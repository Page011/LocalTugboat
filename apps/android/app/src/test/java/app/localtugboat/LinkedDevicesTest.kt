package app.localtugboat

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.tugboat_ffi.DeviceStatus
import uniffi.tugboat_ffi.SnapException
import uniffi.tugboat_ffi.SyncState

/** What the phone shows for 0, 1, 3 and 5 linked PCs: the camera line, banners, alert, Settings rows and the limit. */
class LinkedDevicesTest {
    private val min = 60_000L
    private val hour = 60 * min
    private val day = 24 * hour
    private val now = 10 * day

    private fun dev(name: String, state: SyncState = SyncState.SYNCED, ago: Long? = 10_000, message: String? = null, waiting: UInt = 0u, downloading: UInt = 0u) =
        TestData.device(name, state, message, lastSeenAt = ago?.let { now - it }, lastSyncedAt = if (state == SyncState.SYNCED) ago?.let { now - it } else null, waiting = waiting, downloading = downloading)

    private fun names(n: Int) = listOf("WORK-PC", "HOME-PC", "LAPTOP", "OLD-PC", "SHOP-PC").take(n)
    private fun synced(n: Int) = names(n).map { dev(it) }

    /** The aggregate the core builds: first match wins (refused, syncing, synced, connecting, offline). */
    private fun aggregate(devices: List<DeviceStatus>, waiting: UInt = 0u, storageLow: Boolean = false): uniffi.tugboat_ffi.SyncStatus {
        val live = devices.filter { !it.unlinking }
        val first = { s: SyncState -> live.firstOrNull { it.state == s } }
        val state = when {
            live.isEmpty() -> SyncState.UNPAIRED
            first(SyncState.REFUSED) != null -> SyncState.REFUSED
            first(SyncState.SYNCING) != null -> SyncState.SYNCING
            first(SyncState.SYNCED) != null -> SyncState.SYNCED
            first(SyncState.CONNECTING) != null -> SyncState.CONNECTING
            else -> SyncState.OFFLINE
        }
        val message = first(SyncState.REFUSED)?.message
        val last = live.mapNotNull { it.lastSyncedAt }.maxOrNull()
        return TestData.status(state, waiting = waiting, message = message, storageLow = storageLow, lastSyncedAt = last, devices = devices)
    }

    // ---- 0, 1, 3, 5 devices ----

    @Test fun the_settings_header_counts_linked_pcs_of_the_limit() {
        assertEquals("Linked devices  0 of 5", UiLogic.linkedHeader(emptyList()))
        assertEquals("Linked devices  1 of 5", UiLogic.linkedHeader(synced(1)))
        assertEquals("Linked devices  3 of 5", UiLogic.linkedHeader(synced(3)))
        assertEquals("Linked devices  5 of 5", UiLogic.linkedHeader(synced(5)))
    }

    @Test fun link_a_pc_is_on_until_five_are_linked() {
        for (n in 0..4) {
            val b = UiLogic.linkButton(synced(n))
            assertTrue("$n linked", b.enabled)
            assertNull("$n linked", b.limitLine)
            assertFalse(UiLogic.atLimit(synced(n)))
        }
        val full = UiLogic.linkButton(synced(5))
        assertFalse(full.enabled)
        assertEquals("LIMIT 5 DEVICES - unlink one first", full.limitLine)
        assertTrue(UiLogic.atLimit(synced(5)))
    }

    @Test fun the_limit_here_is_the_limit_in_the_core() {
        // Walk up from the test's working directory to the repository: a missing file fails, it never skips.
        val model = generateSequence(File("").absoluteFile) { it.parentFile }.map { File(it, "crates/core/src/model.rs") }.firstOrNull { it.isFile }
        assertTrue("crates/core/src/model.rs not found above ${File("").absolutePath}", model != null)
        val core = Regex("""pub const MAX_LINKED: u32 = (\d+);""").find(model!!.readText())?.groupValues?.get(1)
        assertEquals(core, Texts.MAX_LINKED.toString())
    }

    @Test fun a_refusal_for_the_limit_reads_as_the_core_says_it() {
        assertEquals("LIMIT 5 DEVICES - unlink one first", Texts.pairFailure(SnapException.TooManyDevices("LIMIT 5 DEVICES - unlink one first"), true))
        // Whatever words the core used, the phone shows one fixed line.
        assertEquals(Texts.LIMIT_LINE, Texts.pairFailure(SnapException.TooManyDevices("x"), true))
    }

    @Test fun the_screen_follows_whether_any_pc_is_linked() {
        assertEquals(Root.ONBOARDING, UiLogic.root(UiState(loaded = true, devices = synced(0))))
        for (n in listOf(1, 3, 5)) assertEquals(Root.MAIN, UiLogic.root(UiState(loaded = true, devices = synced(n))))
        assertFalse(UiLogic.linked(synced(0)))
        assertTrue(UiLogic.linked(synced(1)))
    }

    // ---- the camera line, banner and alert ----

    @Test fun one_linked_pc_keeps_every_current_phone_string() {
        val one = listOf(dev("OFFICE-PC", SyncState.OFFLINE, ago = 3 * day))
        val s = aggregate(one, waiting = 3u)
        assertEquals("PC not found, 3 waiting", Texts.cameraSync(s, now))
        assertEquals("PC NOT FOUND - 3 waiting", UiLogic.syncBanner(s)!!.text)
        assertEquals("PC not found", Texts.cameraSync(aggregate(one), now))
        assertEquals("PC NOT FOUND", UiLogic.syncBanner(aggregate(one))!!.text)
        assertEquals("Synced 10s ago", Texts.cameraSync(aggregate(synced(1)), now))
        assertEquals("Syncing", Texts.cameraSync(aggregate(listOf(dev("OFFICE-PC", SyncState.SYNCING))), now))
        assertEquals("Downloading from PC", UiLogic.fileNote(uniffi.tugboat_ffi.LocalFileState.PENDING, false))
        assertEquals("NOT ON PC", Texts.notOn(listOf("OFFICE-PC"), 1))
        assertEquals(UiLogic.NOTHING_YET, UiLogic.nothingYet(Texts.several(aggregate(synced(1)))))
    }

    @Test fun all_synced_reads_synced_with_no_banner_and_no_alert() {
        val s = aggregate(synced(3))
        assertEquals("Synced 10s ago", Texts.cameraSync(s, now))
        assertEquals(UiLogic.Tone.OK, UiLogic.tone(s))
        assertNull(UiLogic.syncBanner(s))
        assertNull(UiLogic.alert(s))
    }

    @Test fun some_synced_and_some_away_still_reads_synced_and_never_alerts() {
        val devices = listOf(dev("WORK-PC"), dev("HOME-PC", SyncState.OFFLINE, ago = 3 * day), dev("LAPTOP", SyncState.CONNECTING, ago = null))
        val s = aggregate(devices, waiting = 4u)
        assertEquals(SyncState.SYNCED, s.state)
        assertEquals("Synced 10s ago", Texts.cameraSync(s, now))
        assertEquals(UiLogic.Tone.OK, UiLogic.tone(s))
        assertNull(UiLogic.syncBanner(s))
        assertNull(UiLogic.alert(s))
    }

    @Test fun none_reachable_is_the_red_no_pc_found_line_and_still_no_alert() {
        val away = listOf(dev("WORK-PC", SyncState.OFFLINE, ago = hour), dev("HOME-PC", SyncState.CONNECTING, ago = 3 * day), dev("LAPTOP", SyncState.OFFLINE, ago = null))
        val s = aggregate(away, waiting = 3u)
        assertEquals("No PC found, 3 waiting", Texts.cameraSync(s, now))
        assertEquals("No PC found", Texts.cameraSync(aggregate(away), now))
        val banner = UiLogic.syncBanner(s)!!
        assertEquals("NO PC FOUND - 3 waiting", banner.text)
        assertEquals(UiLogic.Level.BLOCKER, banner.level)
        assertEquals("NO PC FOUND", UiLogic.syncBanner(aggregate(away))!!.text)
        assertEquals(UiLogic.Tone.BAD, UiLogic.tone(s))
        assertNull(UiLogic.alert(s)) // being away is normal
    }

    @Test fun any_syncing_pc_reads_syncing() {
        val s = aggregate(listOf(dev("WORK-PC"), dev("HOME-PC", SyncState.SYNCING, downloading = 2u)))
        assertEquals("Syncing", Texts.cameraSync(s, now))
        assertEquals(UiLogic.Tone.BUSY, UiLogic.tone(s))
        assertNull(UiLogic.syncBanner(s))
        assertNull(UiLogic.alert(s))
    }

    @Test fun a_refused_pc_is_named_on_the_line_the_banner_and_the_alert() {
        val text = "UPDATE TUGBOAT on LAPTOP"
        val s = aggregate(listOf(dev("WORK-PC"), dev("LAPTOP", SyncState.REFUSED, ago = 5_000, message = text), dev("HOME-PC", SyncState.OFFLINE, ago = day)))
        assertEquals(SyncState.REFUSED, s.state) // one refusal is not hidden by the PCs that are fine
        assertEquals(text, Texts.cameraSync(s, now))
        assertEquals(text, UiLogic.syncBanner(s)!!.text)
        assertEquals(UiLogic.Level.BLOCKER, UiLogic.syncBanner(s)!!.level)
        assertEquals(text, UiLogic.alert(s))
        assertEquals(UiLogic.Tone.BAD, UiLogic.tone(s))
    }

    @Test fun the_storage_alert_does_not_depend_on_how_many_pcs() {
        for (n in listOf(1, 3, 5)) {
            val s = aggregate(synced(n), storageLow = true)
            assertEquals("PHONE STORAGE LOW - downloads stopped", UiLogic.alert(s))
        }
    }

    @Test fun an_unlinking_row_does_not_make_the_phone_several() {
        val s = aggregate(listOf(dev("WORK-PC", SyncState.OFFLINE), TestData.device("OLD-PC", SyncState.UNPAIRED, unlinking = true)), waiting = 1u)
        assertFalse(Texts.several(s))
        assertEquals("PC not found, 1 waiting", Texts.cameraSync(s, now))
        assertEquals(1, Texts.linkedCount(s.devices))
    }

    // ---- Settings rows ----

    @Test fun a_synced_row_says_when() {
        val line = UiLogic.deviceLine(dev("WORK-PC", ago = 2 * min), now)
        assertEquals("Synced 2m ago", line.text)
        assertFalse(line.problem)
        assertFalse(line.away)
        assertNull(line.waitingChip)
        assertTrue(line.canUnlink)
    }

    @Test fun a_syncing_row_counts_what_is_downloading() {
        assertEquals("Syncing", UiLogic.deviceLine(dev("WORK-PC", SyncState.SYNCING), now).text)
        assertEquals("Syncing - 4 downloading", UiLogic.deviceLine(dev("WORK-PC", SyncState.SYNCING, downloading = 4u), now).text)
    }

    @Test fun an_away_row_is_grey_with_when_it_was_last_seen() {
        val line = UiLogic.deviceLine(dev("HOME-PC", SyncState.OFFLINE, ago = 3 * day), now)
        assertEquals("Not found - last seen 3d ago", line.text)
        assertTrue(line.away)
        assertFalse(line.problem)
        assertEquals("Not found - never seen", UiLogic.deviceLine(dev("NEW-PC", SyncState.CONNECTING, ago = null), now).text)
    }

    @Test fun a_refused_row_shows_the_core_message_as_a_problem() {
        val line = UiLogic.deviceLine(dev("LAPTOP", SyncState.REFUSED, message = "UPDATE TUGBOAT on LAPTOP"), now)
        assertEquals("UPDATE TUGBOAT on LAPTOP", line.text)
        assertTrue(line.problem)
        assertFalse(line.away)
        assertEquals("PC REFUSED THE CONNECTION", UiLogic.deviceLine(dev("LAPTOP", SyncState.REFUSED), now).text)
    }

    @Test fun items_waiting_for_a_pc_show_as_a_chip_on_its_row() {
        assertEquals("3 waiting", UiLogic.deviceLine(dev("WORK-PC", waiting = 3u), now).waitingChip)
        assertEquals("12 waiting", UiLogic.deviceLine(dev("HOME-PC", SyncState.OFFLINE, ago = day, waiting = 12u), now).waitingChip)
        assertNull(UiLogic.deviceLine(dev("WORK-PC", waiting = 0u), now).waitingChip)
    }

    @Test fun an_unlinking_row_has_no_button_and_says_it_will_tell_the_pc() {
        val line = UiLogic.deviceLine(TestData.device("OLD-PC", SyncState.UNPAIRED, waiting = 5u, unlinking = true), now)
        assertEquals("Unlinking - tells it when reachable", line.text)
        assertFalse(line.canUnlink)
        assertNull(line.waitingChip)
        assertFalse(line.problem)
    }

    @Test fun an_unlinking_row_is_not_counted_toward_the_limit() {
        val five = synced(4) + TestData.device("OLD-PC", SyncState.UNPAIRED, unlinking = true)
        assertEquals("Linked devices  4 of 5", UiLogic.linkedHeader(five))
        assertTrue(UiLogic.linkButton(five).enabled)
        assertFalse(UiLogic.atLimit(five))
    }

    // ---- bin note ----

    @Test fun the_bin_note_names_one_missing_pc_or_counts_several() {
        assertEquals("NOT ON PC", Texts.notOn(listOf("WORK-PC"), 1))
        assertEquals("NOT ON WORK-PC", Texts.notOn(listOf("WORK-PC"), 3))
        assertEquals("NOT ON 2 PCS", Texts.notOn(listOf("WORK-PC", "HOME-PC"), 3))
        assertEquals("NOT ON 5 PCS", Texts.notOn(names(5), 5))
        assertEquals("NOT ON PC", Texts.notOn(emptyList(), 3))
    }

    // ---- background work ----

    @Test fun the_radio_searches_while_a_linked_pc_is_not_found() {
        assertFalse(UiLogic.searching(null))
        assertFalse(UiLogic.searching(aggregate(synced(3))))
        assertTrue(UiLogic.searching(aggregate(listOf(dev("WORK-PC"), dev("HOME-PC", SyncState.OFFLINE)))))
        assertTrue(UiLogic.searching(aggregate(listOf(dev("WORK-PC", SyncState.CONNECTING)))))
        assertFalse(UiLogic.searching(aggregate(listOf(dev("WORK-PC", SyncState.REFUSED, message = "x")))))
        // Not for a PC that is only waiting to be told, and not while paused.
        assertFalse(UiLogic.searching(aggregate(listOf(dev("WORK-PC"), TestData.device("OLD-PC", SyncState.OFFLINE, unlinking = true)))))
        assertFalse(UiLogic.searching(TestData.status(SyncState.PAUSED, devices = listOf(dev("WORK-PC", SyncState.OFFLINE)))))
    }

    @Test fun a_catch_up_is_done_when_a_pc_synced_after_it_began_and_nothing_connected_still_has_files() {
        val started = now - 15_000
        assertTrue(UiLogic.caughtUp(aggregate(synced(1)), started))
        assertTrue(UiLogic.caughtUp(aggregate(synced(3)), started))
        // The last sync is older than the run: stale status from an earlier session.
        assertFalse(UiLogic.caughtUp(aggregate(listOf(dev("WORK-PC", ago = 10 * min))), started))
        assertFalse(UiLogic.caughtUp(null, started))
        // A connected PC still has files for this phone.
        assertFalse(UiLogic.caughtUp(aggregate(listOf(dev("WORK-PC", downloading = 2u), dev("HOME-PC"))), started))
        // A PC that is away cannot serve anything, so its files do not hold the run up.
        assertTrue(UiLogic.caughtUp(aggregate(listOf(dev("WORK-PC"), dev("HOME-PC", SyncState.OFFLINE, ago = day, downloading = 3u))), started))
        // Still syncing is not done.
        assertFalse(UiLogic.caughtUp(aggregate(listOf(dev("WORK-PC"), dev("HOME-PC", SyncState.SYNCING))), started))
    }
}
