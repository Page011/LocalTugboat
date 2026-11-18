package app.localtugboat

import java.time.Instant
import java.time.ZoneId
import java.util.Locale
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.tugboat_ffi.ActivityAction
import uniffi.tugboat_ffi.ItemInfo
import uniffi.tugboat_ffi.ItemStateInfo
import uniffi.tugboat_ffi.LocalFileState
import uniffi.tugboat_ffi.MediaKind
import uniffi.tugboat_ffi.PairedPc
import uniffi.tugboat_ffi.SyncState
import uniffi.tugboat_ffi.SyncStatus

class UiLogicTest {
    private val hour = 3_600_000L
    private val day = 24 * hour

    private fun perms(camera: Boolean = true, mic: Boolean = false, loc: LocationAccess = LocationAccess.NONE, notif: Boolean = true, net: Boolean = true) =
        PermState(camera, mic, loc, notif, net, batteryExempt = false)

    private fun pc() = PairedPc("a".repeat(64), "NIGEL-PC", 1L, 2L, listOf("192.168.1.5:47300"))

    private fun status(state: SyncState, waiting: UInt = 0u, message: String? = null) = SyncStatus(state, "NIGEL-PC", null, waiting, 0u, message)

    private fun item(
        state: ItemStateInfo = ItemStateInfo.Live(1_000_000L + 5 * hour), local: LocalFileState = LocalFileState.PRESENT,
        pinned: Boolean = false, kind: MediaKind = MediaKind.IMAGE, path: String? = "/x/a.jpg",
    ) = ItemInfo("i1", "a", "f1", kind, 10uL, "jpg", 0L, pinned, state, local, path, null)

    // ---- which screen ----

    @Test fun root_picks_loading_fatal_onboarding_main_in_that_order() {
        assertEquals(Root.LOADING, UiLogic.root(UiState(loaded = false, openError = "x", paired = pc())))
        assertEquals(Root.FATAL, UiLogic.root(UiState(loaded = true, openError = "KEY UNREADABLE", paired = pc())))
        assertEquals(Root.ONBOARDING, UiLogic.root(UiState(loaded = true)))
        assertEquals(Root.MAIN, UiLogic.root(UiState(loaded = true, paired = pc())))
    }

    // ---- permissions ----

    @Test fun local_network_row_exists_only_from_api_37_and_the_microphone_row_only_when_asked_for() {
        val p = perms()
        assertFalse(UiLogic.permRows(p, emptySet(), 36).any { it.kind == PermKind.LOCAL_NETWORK })
        assertTrue(UiLogic.permRows(p, emptySet(), 37).any { it.kind == PermKind.LOCAL_NETWORK })
        assertFalse(UiLogic.permRows(p, emptySet(), 37).any { it.kind == PermKind.MICROPHONE })
        assertTrue(UiLogic.permRows(p, emptySet(), 37, withMic = true).any { it.kind == PermKind.MICROPHONE })
    }

    @Test fun a_denied_row_reads_needed_until_the_system_stops_asking_then_blocked() {
        val p = perms(camera = false)
        assertEquals(PermStatus.NEEDED, UiLogic.permRows(p, emptySet(), 37).first { it.kind == PermKind.CAMERA }.status)
        assertEquals(PermStatus.BLOCKED, UiLogic.permRows(p, setOf(PermKind.CAMERA), 37).first { it.kind == PermKind.CAMERA }.status)
        assertEquals(PermStatus.GRANTED, UiLogic.permRows(perms(), setOf(PermKind.CAMERA), 37).first { it.kind == PermKind.CAMERA }.status)
        assertTrue(UiLogic.opensSettings(PermStatus.BLOCKED))
        assertFalse(UiLogic.opensSettings(PermStatus.NEEDED))
    }

    @Test fun only_camera_and_local_network_are_required() {
        val rows = UiLogic.permRows(perms(notif = false, loc = LocationAccess.NONE), emptySet(), 37)
        assertEquals(setOf(PermKind.CAMERA, PermKind.LOCAL_NETWORK), rows.filter { it.required }.map { it.kind }.toSet())
        assertTrue(UiLogic.requiredMet(perms(notif = false)))
        assertFalse(UiLogic.requiredMet(perms(camera = false)))
        assertFalse(UiLogic.requiredMet(perms(net = false)))
    }

    @Test fun permission_screen_shows_until_required_are_on_and_nothing_askable_is_left_or_the_user_continued() {
        fun rows(p: PermState, hard: Set<PermKind> = emptySet()) = UiLogic.permRows(p, hard, 37)
        val all = perms(loc = LocationAccess.PRECISE)
        assertFalse(UiLogic.showPermissionScreen(rows(all), false))
        assertTrue(UiLogic.showPermissionScreen(rows(perms(camera = false, loc = LocationAccess.PRECISE)), true)) // required missing: Continue never skips it
        assertTrue(UiLogic.showPermissionScreen(rows(perms()), false)) // optional location still askable
        assertFalse(UiLogic.showPermissionScreen(rows(perms()), true)) // user pressed Continue
        assertFalse(UiLogic.showPermissionScreen(rows(perms(), setOf(PermKind.LOCATION)), false)) // location blocked: nothing left to ask
    }

    @Test fun onboarding_step_follows_pairing_state_then_permissions() {
        val granted = UiLogic.permRows(perms(loc = LocationAccess.PRECISE), emptySet(), 37)
        assertEquals(OnboardingStep.PERMISSIONS, UiLogic.onboardingStep(null, false, PairUi.Idle))
        assertEquals(OnboardingStep.SCAN, UiLogic.onboardingStep(granted, false, PairUi.Idle))
        assertEquals(OnboardingStep.CONNECTING, UiLogic.onboardingStep(null, false, PairUi.Dialing))
        assertEquals(OnboardingStep.CODE, UiLogic.onboardingStep(granted, false, PairUi.Code("482913", "PC")))
        assertEquals(OnboardingStep.FAILED, UiLogic.onboardingStep(granted, false, PairUi.Failed("x")))
        assertEquals(OnboardingStep.DONE, UiLogic.onboardingStep(granted, false, PairUi.Done("PC")))
    }

    @Test fun pairing_code_is_grouped_for_comparison_with_the_pc() {
        assertEquals("482 913", UiLogic.formatCode("482913"))
        assertEquals("482 913", UiLogic.formatCode(" 482913 "))
        assertEquals("482 913", UiLogic.formatCode("482 913"))
        assertEquals("12", UiLogic.formatCode("12"))
    }

    @Test fun only_a_blocked_local_network_offers_allow() {
        assertEquals(FailAction.ALLOW, UiLogic.failAction("LOCAL NETWORK BLOCKED - allow it, then scan again"))
        assertEquals(FailAction.RETRY, UiLogic.failAction("PC NOT FOUND - same Wi-Fi as the PC?"))
        assertEquals(FailAction.RETRY, UiLogic.failAction("PAIRING CODE EXPIRED - show a new QR"))
    }

    @Test fun permission_names_round_trip_to_their_kind() {
        for (k in PermKind.entries) {
            val names = UiLogic.permissionsFor(k)
            assertTrue(names.isNotEmpty())
            assertTrue("$k", names.all { UiLogic.permKind(it) == k })
        }
        assertNull(UiLogic.permKind("android.permission.READ_CONTACTS"))
    }

    @Test fun hard_denied_needs_no_rationale_or_notifications_and_clears_when_granted() {
        val none = perms(camera = false, net = false, notif = false)
        assertEquals(setOf(PermKind.CAMERA), UiLogic.hardDenied(emptySet(), setOf(PermKind.CAMERA), setOf(PermKind.CAMERA), none))
        // First denial: the system still has a rationale, so the dialog can come again.
        assertEquals(emptySet<PermKind>(), UiLogic.hardDenied(emptySet(), setOf(PermKind.CAMERA), emptySet(), none))
        // Notifications can be off with the permission itself granted: no dialog helps, only settings.
        assertEquals(setOf(PermKind.NOTIFICATIONS), UiLogic.hardDenied(emptySet(), setOf(PermKind.NOTIFICATIONS), emptySet(), none))
        // Granted later (from settings): dropped.
        assertEquals(emptySet<PermKind>(), UiLogic.hardDenied(setOf(PermKind.CAMERA), emptySet(), emptySet(), perms()))
        // Unrelated earlier entries stay.
        assertEquals(setOf(PermKind.LOCATION), UiLogic.hardDenied(setOf(PermKind.LOCATION), setOf(PermKind.CAMERA), emptySet(), none))
    }

    // ---- banners ----

    @Test fun banners_list_blockers_first_each_group_in_a_fixed_order() {
        val ui = UiState(
            loaded = true, paired = pc(), perms = perms(notif = false, net = false), stoppedGapFromMs = 1_000L,
            storageLow = true, errors = listOf("old", "download failed"),
        )
        val ids = UiLogic.banners(ui, ZoneId.of("UTC")).map { it.id }
        assertEquals(
            listOf(UiLogic.BannerId.LOCAL_NETWORK, UiLogic.BannerId.STORAGE, UiLogic.BannerId.STOPPED, UiLogic.BannerId.NOTIFICATIONS, UiLogic.BannerId.ERROR),
            ids,
        )
        val b = UiLogic.banners(ui, ZoneId.of("UTC"))
        assertEquals(listOf(UiLogic.Level.BLOCKER, UiLogic.Level.BLOCKER, UiLogic.Level.WARNING, UiLogic.Level.WARNING, UiLogic.Level.WARNING), b.map { it.level })
        assertEquals("download failed", b.last().text) // newest error only
        assertEquals("Fix", b.first { it.id == UiLogic.BannerId.STOPPED }.action)
    }

    @Test fun an_unpaired_phone_gets_no_permission_or_storage_banners() {
        val ui = UiState(loaded = true, perms = perms(notif = false, net = false), storageLow = true)
        assertTrue(UiLogic.banners(ui, ZoneId.of("UTC")).isEmpty())
    }

    @Test fun a_core_that_will_not_open_is_a_blocker() {
        val b = UiLogic.banners(UiState(loaded = true, openError = "KEY UNREADABLE"), ZoneId.of("UTC")).single()
        assertEquals(UiLogic.Level.BLOCKER, b.level)
        assertNull(b.action)
    }

    @Test fun unreachable_pc_is_a_red_banner_not_an_empty_screen() {
        val off = UiLogic.syncBanner(status(SyncState.OFFLINE, waiting = 3u))!!
        assertEquals("PC NOT FOUND - 3 waiting", off.text)
        assertEquals(UiLogic.Level.BLOCKER, off.level)
        assertEquals("PC NOT FOUND", UiLogic.syncBanner(status(SyncState.OFFLINE))!!.text)
        assertEquals("PC NOT FOUND", UiLogic.syncBanner(status(SyncState.CONNECTING))!!.text) // same words as the sync line
        assertEquals("Update Tugboat on the PC", UiLogic.syncBanner(status(SyncState.REFUSED, message = "Update Tugboat on the PC"))!!.text)
        assertEquals(UiLogic.Level.BLOCKER, UiLogic.syncBanner(status(SyncState.PAUSED))!!.level)
        assertNull(UiLogic.syncBanner(status(SyncState.SYNCED)))
        assertNull(UiLogic.syncBanner(status(SyncState.SYNCING)))
        assertNull(UiLogic.syncBanner(null))
    }

    @Test fun sync_dot_is_red_for_unreachable_connecting_paused_refused() {
        assertEquals(UiLogic.Tone.OK, UiLogic.tone(status(SyncState.SYNCED)))
        assertEquals(UiLogic.Tone.BUSY, UiLogic.tone(status(SyncState.SYNCING)))
        assertEquals(UiLogic.Tone.BAD, UiLogic.tone(status(SyncState.OFFLINE)))
        assertEquals(UiLogic.Tone.BAD, UiLogic.tone(status(SyncState.PAUSED)))
        assertEquals(UiLogic.Tone.BAD, UiLogic.tone(status(SyncState.REFUSED)))
        assertEquals(UiLogic.Tone.BAD, UiLogic.tone(status(SyncState.CONNECTING)))
        assertEquals(UiLogic.Tone.WARN, UiLogic.tone(status(SyncState.UNPAIRED)))
        assertEquals(UiLogic.Tone.WARN, UiLogic.tone(null))
    }

    // ---- camera ----

    @Test fun photo_flash_cycles_off_auto_on_and_video_has_a_torch_only() {
        assertEquals(UiLogic.Flash.AUTO, UiLogic.nextFlash(UiLogic.Flash.OFF, false))
        assertEquals(UiLogic.Flash.ON, UiLogic.nextFlash(UiLogic.Flash.AUTO, false))
        assertEquals(UiLogic.Flash.OFF, UiLogic.nextFlash(UiLogic.Flash.ON, false))
        assertEquals(UiLogic.Flash.ON, UiLogic.nextFlash(UiLogic.Flash.OFF, true))
        assertEquals(UiLogic.Flash.OFF, UiLogic.nextFlash(UiLogic.Flash.ON, true))
        assertEquals(UiLogic.Flash.ON, UiLogic.nextFlash(UiLogic.Flash.AUTO, true)) // AUTO is a photo state: never stuck in video
    }

    @Test fun lens_chips_follow_the_zoom_range() {
        assertEquals(listOf(0.6f, 1f, 2f, 5f), UiLogic.lensChips(0.6f, 10f))
        assertEquals(listOf(1f, 2f), UiLogic.lensChips(1f, 3f))
        assertEquals(listOf(1f), UiLogic.lensChips(1f, 1.5f))
        assertEquals(listOf(1f, 2f, 5f), UiLogic.lensChips(0.96f, 8f)) // 0.96 is "1x" to the eye
    }

    @Test fun zoom_label_drops_a_trailing_zero() {
        assertEquals("1x", UiLogic.zoomLabel(1f))
        assertEquals("0.6x", UiLogic.zoomLabel(0.6f))
        assertEquals("2x", UiLogic.zoomLabel(2.04f))
        assertEquals("2.5x", UiLogic.zoomLabel(2.5f))
    }

    @Test fun recording_clock() {
        assertEquals("0:00", UiLogic.clock(-5))
        assertEquals("0:07", UiLogic.clock(7_400))
        assertEquals("12:30", UiLogic.clock(750_000))
        assertEquals("1:02:03", UiLogic.clock(3_723_000))
    }

    @Test fun folder_chip_says_where_the_shot_goes_and_flags_no_location() {
        assertEquals("Inbox", UiLogic.chip(null).text)
        assertFalse(UiLogic.chip(null).noLocation)
        assertEquals("Work", UiLogic.chip(Target("f", "Work", false)).text)
        val nl = UiLogic.chip(Target("u", "Unsorted", true))
        assertEquals("No location", nl.text)
        assertTrue(nl.noLocation)
    }

    @Test fun location_is_stored_on_a_shot_only_when_fresh_and_accurate() {
        assertTrue(UiLogic.usableFix(50.0, 119_999))
        assertTrue(UiLogic.usableFix(100.0, 0))
        assertFalse(UiLogic.usableFix(100.1, 0))
        assertFalse(UiLogic.usableFix(50.0, 120_000))
        assertFalse(UiLogic.usableFix(50.0, -1))
        assertFalse(UiLogic.usableFix(0.0, 10))
    }

    // ---- gallery ----

    @Test fun list_states_keep_rows_over_a_load_error_and_say_why_a_list_is_empty() {
        assertEquals(UiLogic.ListUi.Content, UiLogic.listUi(3, true, "boom", null, false))
        assertEquals(UiLogic.ListUi.Failed("boom"), UiLogic.listUi(0, true, "boom", null, false))
        assertEquals(UiLogic.ListUi.Failed("boom"), UiLogic.listUi(0, false, "boom", null, false))
        assertEquals(UiLogic.ListUi.Loading, UiLogic.listUi(0, false, null, null, false))
        assertEquals(UiLogic.ListUi.Empty("Bin is empty."), UiLogic.listUi(0, true, null, null, true))
        assertEquals(UiLogic.ListUi.Empty("Nothing in Work."), UiLogic.listUi(0, true, null, "Work", false))
        assertEquals(UiLogic.ListUi.Empty("Nothing yet. Photos you take here show up on your PC."), UiLogic.listUi(0, true, null, null, false))
    }

    @Test fun retention_words() {
        assertEquals("Kept", UiLogic.retention(null))
        assertEquals("Expires after 7d", UiLogic.retention(604_800uL))
        assertEquals("Expires after 24h", UiLogic.retention(86_400uL))
    }

    @Test fun file_notes_explain_a_missing_file_in_words() {
        assertEquals("Downloading from PC", UiLogic.fileNote(LocalFileState.PENDING, false))
        assertEquals("FILE MISSING on this phone", UiLogic.fileNote(LocalFileState.LOST, false))
        assertEquals("PC refused this file", UiLogic.fileNote(LocalFileState.REFUSED, false))
        assertEquals("On PC only", UiLogic.fileNote(LocalFileState.PURGED, false))
        assertNull(UiLogic.fileNote(LocalFileState.PURGED, true))
        assertNull(UiLogic.fileNote(LocalFileState.PRESENT, false))
        assertNull(UiLogic.fileNote(LocalFileState.BINNED, true))
    }

    @Test fun cell_label_badges_and_waiting() {
        val now = 1_000_000L
        val live = UiLogic.cell(item(), now, 7u, false)
        assertEquals("Expires in 5h", live.label)
        assertFalse(live.kept)
        assertFalse(live.video)
        assertTrue(UiLogic.cell(item(pinned = true), now, 7u, false).kept)
        assertTrue(UiLogic.cell(item(state = ItemStateInfo.FolderKeep), now, 7u, false).kept)
        assertEquals("Kept (folder)", UiLogic.cell(item(state = ItemStateInfo.FolderKeep), now, 7u, false).label)
        val waiting = UiLogic.cell(item(state = ItemStateInfo.Waiting, kind = MediaKind.VIDEO), now, 7u, false)
        assertEquals("Waiting for PC", waiting.label)
        assertTrue(waiting.waiting)
        assertTrue(waiting.video)
        // A file problem outranks the expiry text.
        assertEquals("FILE MISSING on this phone", UiLogic.cell(item(local = LocalFileState.LOST), now, 7u, false).label)
        // Bin rows count their days.
        val binned = item(state = ItemStateInfo.Deleted(now - day), local = LocalFileState.BINNED)
        assertEquals("In Bin, 6d left", UiLogic.cell(binned, now, 7u, true).label)
    }

    @Test fun share_and_save_need_the_file_on_this_phone() {
        assertTrue(UiLogic.fileHeld(item()))
        assertFalse(UiLogic.fileHeld(item(path = null)))
        assertFalse(UiLogic.fileHeld(item(local = LocalFileState.PENDING)))
        assertFalse(UiLogic.fileHeld(item(local = LocalFileState.BINNED)))
    }

    @Test fun zoomed_photo_cannot_be_dragged_past_its_edge() {
        assertEquals(0f, UiLogic.panLimit(1f, 1000f), 0f)
        assertEquals(0f, UiLogic.panLimit(0.5f, 1000f), 0f)
        assertEquals(500f, UiLogic.panLimit(2f, 1000f), 0f)
        assertEquals(1000f, UiLogic.panLimit(3f, 1000f), 0f)
    }

    // ---- times and numbers ----

    @Test fun capture_time_is_in_the_given_zone_and_language() {
        val ms = Instant.parse("2026-11-17T10:30:00Z").toEpochMilli()
        assertEquals("17 Nov, 10:30", UiLogic.capturedAt(ms, ZoneId.of("UTC"), Locale.ENGLISH))
        assertEquals("17 Nov, 21:30", UiLogic.capturedAt(ms, ZoneId.of("Australia/Sydney"), Locale.ENGLISH))
    }

    @Test fun activity_time_is_relative_inside_a_day_then_a_date() {
        val now = Instant.parse("2026-11-17T10:30:00Z").toEpochMilli()
        val utc = ZoneId.of("UTC")
        assertEquals("5m ago", UiLogic.activityTime(now - 5 * 60_000, now, utc, Locale.ENGLISH))
        assertEquals("23h ago", UiLogic.activityTime(now - 23 * hour, now, utc, Locale.ENGLISH))
        assertEquals("15 Nov, 10:30", UiLogic.activityTime(now - 2 * day, now, utc, Locale.ENGLISH))
    }

    @Test fun every_activity_action_has_its_own_text_and_problems_are_flagged() {
        val texts = ActivityAction.entries.map { UiLogic.activityText(it) }
        assertTrue(texts.all { it.isNotBlank() })
        assertEquals(ActivityAction.entries.size, texts.toSet().size)
        val problems = ActivityAction.entries.filter { UiLogic.activityIsProblem(it) }.toSet()
        assertEquals(setOf(ActivityAction.REFUSED, ActivityAction.CONFLICT, ActivityAction.LOST, ActivityAction.SYNC_ERROR, ActivityAction.PAUSED), problems)
        assertEquals("SYNC ERROR", UiLogic.activityText(ActivityAction.SYNC_ERROR)) // the key word is capitalised
    }

    @Test fun storage_bytes_text() {
        assertEquals("12.3 GB", UiLogic.bytes((12.3 * (1L shl 30)).toLong()))
        assertEquals("2.0 GB", UiLogic.bytes(2L shl 30))
        assertEquals("850 MB", UiLogic.bytes(850L shl 20))
        assertEquals("100 KB", UiLogic.bytes(100L shl 10))
    }

    // ---- shutter folder (the chip's target may be stale for a moment after a pick) ----

    private val ids = listOf("inbox", "keep", "site")

    @Test fun an_explicit_pick_that_exists_wins_even_over_a_target_computed_for_another_pick() {
        assertEquals("keep", UiLogic.shotFolder("keep", ids, "inbox", null))
        assertEquals("keep", UiLogic.shotFolder("keep", ids, "site", "site")) // stale target for an older pick
    }

    @Test fun a_deleted_pick_uses_the_target_computed_for_that_same_pick() {
        assertEquals("inbox", UiLogic.shotFolder("gone", ids, "inbox", "gone"))
    }

    @Test fun a_fresh_target_for_automatic_is_used() {
        assertEquals("site", UiLogic.shotFolder(null, ids, "site", null))
    }

    @Test fun a_stale_target_with_automatic_is_not_used_so_the_place_rule_decides_at_hand_off() {
        assertNull(UiLogic.shotFolder(null, ids, "keep", "keep")) // the pick was just cleared, target is still the old pick's
        assertNull(UiLogic.shotFolder(null, ids, null, null)) // no target yet
        assertNull(UiLogic.shotFolder("gone", ids, "inbox", "other")) // pick deleted and target for something else
    }

    // ---- saved viewer state ----

    @Test fun a_viewer_that_left_the_stack_loses_its_saved_state_and_others_keep_it() {
        val a = Route.Viewer(null, false, "a")
        val c = Route.Viewer(null, false, "c")
        val seen = setOf<Route>(Route.Camera, Route.Gallery(null), a, c)
        // a was popped, c is still on the stack (under Settings): only a is dropped.
        assertEquals(setOf(a.toString()), UiLogic.droppedViewerKeys(seen, listOf(Route.Camera, c, Route.Settings)))
        // the same viewer opened again is on the stack: kept.
        assertTrue(UiLogic.droppedViewerKeys(seen, listOf(Route.Camera, Route.Gallery(null), a, c)).isEmpty())
        // non-viewer routes are never dropped, even when gone.
        assertTrue(UiLogic.droppedViewerKeys(setOf(Route.Gallery(null), Route.Settings, Route.Bin), listOf(Route.Camera)).isEmpty())
    }

    @Test fun bin_choices_always_include_the_current_value() {
        assertEquals(listOf(1u, 3u, 7u, 14u, 30u), UiLogic.binChoices(7u))
        assertEquals(listOf(1u, 3u, 7u, 10u, 14u, 30u), UiLogic.binChoices(10u))
    }

    // ---- R3: a deleted folder is never handed to the core by a cached target ----

    @Test fun a_cached_target_whose_folder_was_deleted_is_not_used() {
        assertNull(UiLogic.shotFolder(null, ids, "gone", null)) // automatic, target cached for automatic, folder deleted since
        assertNull(UiLogic.shotFolder("gone", ids, "gone", "gone")) // deleted pick, target cached for that pick, also deleted
    }

    // ---- R4: the full-size photo is decoded again when the held file's content changes ----

    @Test fun the_photo_decode_key_follows_path_size_and_thumbnail() {
        val base = UiLogic.photoDecodeKey("items/a.jpg", 100uL, "thumbs/h1.jpg")
        assertEquals(base, UiLogic.photoDecodeKey("items/a.jpg", 100uL, "thumbs/h1.jpg"))
        assertFalse(base == UiLogic.photoDecodeKey("items/a.jpg", 101uL, "thumbs/h1.jpg")) // edited, size changed
        assertFalse(base == UiLogic.photoDecodeKey("items/a.jpg", 100uL, "thumbs/h2.jpg")) // edited, same size, new thumbnail
        assertFalse(base == UiLogic.photoDecodeKey("items/b.jpg", 100uL, "thumbs/h1.jpg"))
    }
}
