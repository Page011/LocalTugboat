package app.localtugboat

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.tugboat_ffi.ActivityAction
import uniffi.tugboat_ffi.ItemStateInfo

/**
 * Tests for the verifier's findings that were fixed. Logic is tested directly; the rest reads the sources and the
 * manifest like AdversarialTest does, because it needs an Android runtime (the emulator runs are in
 * notes/android-results.md).
 */
class FixesTest {
    private val main: File = listOf(File("src/main"), File("app/src/main")).first { it.isDirectory }
    private val kotlin = File(main, "java/app/localtugboat")
    private fun source(name: String) = File(kotlin, name).readText()
    private val manifest get() = File(main, "AndroidManifest.xml").readText()

    // ---- battery: no direct exemption request (Play policy) ----

    @Test fun the_manifest_does_not_ask_for_the_battery_exemption_permission() {
        assertFalse(manifest.contains("REQUEST_IGNORE_BATTERY_OPTIMIZATIONS"))
    }

    @Test fun the_fix_flow_opens_the_optimisation_list_not_the_request_dialog() {
        val fix = source("BatteryFix.kt")
        assertTrue(fix.contains("Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)"))
        assertFalse("direct request action still used", Regex("""Intent\(Settings\.ACTION_REQUEST_IGNORE""").containsMatchIn(fix))
    }

    @Test fun a_phone_without_the_optimisation_list_still_reaches_the_vendor_screen() {
        assertTrue(source("MainActivity.kt").contains("catch (e: ActivityNotFoundException)"))
    }

    // ---- share target survives recreation ----

    @Test fun share_activity_handles_every_resize_style_change_itself() {
        val tag = manifest.substringAfter("android:name=\".ShareActivity\"").substringBefore(">")
        val changes = Regex("""configChanges="([^"]+)"""").find(tag)!!.groupValues[1].split("|").toSet()
        val needed = setOf("orientation", "screenSize", "smallestScreenSize", "screenLayout", "density", "fontScale", "locale", "layoutDirection", "uiMode", "keyboardHidden")
        assertEquals("missing: ${needed - changes}", emptySet<String>(), needed - changes)
    }

    @Test fun share_activity_finishes_through_the_process_level_flag_not_through_this() {
        val src = source("ShareActivity.kt")
        assertTrue(src.contains("ShareRuns.active.first { it == 0 }"))
        // The process-scope coroutine must not call finish() on the instance that started it.
        val coroutine = src.substringAfter("app.core.scope.launch").substringBefore("lifecycleScope.launch")
        assertFalse("the import coroutine finishes a possibly destroyed activity", coroutine.contains("finish()"))
        assertTrue("a recreated instance must not import again", src.contains("if (savedInstanceState == null)"))
    }

    // ---- camera ----

    @Test fun the_camera_darkens_the_status_bar_strip() {
        assertTrue(source("CameraScreen.kt").contains("windowInsetsTopHeight(WindowInsets.statusBars)"))
    }

    @Test fun the_shutter_sits_on_a_scrim_and_disabled_is_not_faded_white() {
        val shutter = source("CameraScreen.kt").substringAfter("private fun Shutter(")
        assertTrue(shutter.contains("background(ShutterDisc, CircleShape)"))
        assertFalse("disabled shutter is white at partial alpha", shutter.contains("copy(alpha"))
    }

    @Test fun the_folder_chip_is_in_the_bottom_cluster_not_the_top_row() {
        val cam = source("CameraScreen.kt")
        val firstBottom = cam.indexOf("// ---- bottom, in reach of the thumb ----")
        val chipCall = cam.indexOf("FolderChip(UiLogic.chip(target)")
        assertTrue(firstBottom in 0 until chipCall)
    }

    @Test fun the_disabled_shutter_grey_is_at_least_3_to_1_on_the_worst_scrim() {
        fun lin(v: Int): Double = (v / 255.0).let { if (it <= 0.03928) it / 12.92 else Math.pow((it + 0.055) / 1.055, 2.4) }
        fun lum(v: Int) = 0.2126 * lin(v) + 0.7152 * lin(v) + 0.0722 * lin(v)
        val scrimOverWhite = (0.4 * 255).toInt() // 60% black over a white scene
        val grey = 0xC0
        val r = (lum(grey) + 0.05) / (lum(scrimOverWhite) + 0.05)
        assertTrue("grey on scrim %.2f".format(r), r >= 3.0)
    }

    // ---- text at large font sizes ----

    @Test fun every_text_with_maxLines_ellipsizes() {
        val bad = kotlin.listFiles { f -> f.extension == "kt" }.orEmpty().flatMap { f ->
            f.readLines().mapIndexedNotNull { i, line ->
                if (line.contains("maxLines") && !line.contains("TextOverflow.Ellipsis")) "${f.name}:${i + 1}" else null
            }
        }
        assertTrue("maxLines without overflow = Ellipsis: $bad", bad.isEmpty())
    }

    @Test fun the_viewer_action_row_gives_each_action_an_equal_slot_and_drops_words_at_large_fonts() {
        val v = source("Viewer.kt")
        assertTrue(v.contains("Modifier.weight(1f)"))
        assertTrue(v.contains("fontScale <= 1.5f"))
    }

    // ---- delete is undoable ----

    @Test fun delete_tells_the_user_where_it_went_and_offers_undo() {
        assertTrue(source("SnapViewModel.kt").contains("_binned.tryEmit(id)"))
        val shell = source("Shell.kt")
        assertTrue(shell.contains("\"Moved to Bin\"") && shell.contains("actionLabel = \"Undo\"") && shell.contains("vm.restore(id)"))
    }

    // ---- words ----

    @Test fun every_problem_activity_row_is_in_capitals() {
        val problems = ActivityAction.values().filter { UiLogic.activityIsProblem(it) }
        assertTrue(problems.isNotEmpty())
        for (a in problems) {
            val t = UiLogic.activityText(a)
            assertEquals("$a reads '$t'", t.uppercase(), t)
        }
    }

    @Test fun keep_labels_cannot_be_read_as_a_folder_name() {
        val now = 1_000_000L
        assertEquals("Kept", Texts.expiryLabel(ItemStateInfo.Kept, now, 7u))
        assertEquals("Kept (folder)", Texts.expiryLabel(ItemStateInfo.FolderKeep, now, 7u))
        assertNotEquals("Keep", Texts.expiryLabel(ItemStateInfo.FolderKeep, now, 7u))
    }

    // ---- background ----

    @Test fun the_worker_retries_orphaned_shots_too() {
        assertTrue(source("Background.kt").contains("Capture.recover("))
        assertTrue(source("SnapViewModel.kt").contains("Capture.recover("))
    }

    @Test fun recover_skips_a_file_a_hand_off_is_still_working_on() {
        val imp = source("Import.kt")
        assertTrue(imp.contains("f.path in inFlight"))
        assertTrue(imp.contains("inFlight.add(src.path)"))
    }

    @Test fun the_deep_link_guard_and_the_pair_button_both_ask_the_core() {
        val vm = source("SnapViewModel.kt")
        val link = vm.substringAfter("fun onDeepLink(").substringBefore("fun dismissDeepLink(")
        assertEquals(2, Regex("""pairedPc\(\) != null""").findAll(link).count())
        assertFalse("meta.value.paired decides it", link.contains("meta.value.paired"))
    }

    // Found on the emulator while testing Undo: a list reload waited 150 ms after ItemsChanged, but the Sync event
    // that follows within 20 ms cancelled the wait (collectLatest cancels on EVERY value), so the grid never reloaded.
    @Test fun list_reload_filters_events_before_collectLatest() {
        val g = source("Gallery.kt").substringAfter("fun ReloadOnEvents(").substringBefore("fun itemsRelevant")
        assertTrue(g.contains("vm.events.filter(relevant).collectLatest"))
    }
}
