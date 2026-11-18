package app.localtugboat

import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import uniffi.tugboat_ffi.MediaKind

/**
 * 0.2.0 "any file type" on the phone: share names, refusals, file tiles' labels, MIME, the real name other apps see.
 * The core's own name rules live in Rust; [FakeRules] stands in for the native library (a JVM test cannot load an
 * Android .so), so only an emulator run proves the real rules end to end.
 */
class AnyFileTest {
    private val main: File = listOf(File("src/main"), File("app/src/main")).first { it.isDirectory }
    private val kotlin = File(main, "java/app/localtugboat")

    /** Same rule as core `names::ext_of` / `is_refused_name`, only as far as these tests need. */
    internal object FakeRules : NameRules {
        private val ok = Regex("^[a-z0-9_-]{1,16}$")
        private val refusedSuffix = listOf(".lnk", ".url", ".scf", ".library-ms", ".searchconnector-ms")
        override fun extOf(name: String): String {
            val n = name.trimEnd(' ', '.')
            val i = n.lastIndexOf('.')
            if (i <= 0) return ""
            val e = n.substring(i + 1).lowercase()
            return if (ok.matches(e)) e else ""
        }
        override fun isRefused(name: String): Boolean {
            val n = name.trimEnd(' ', '.').lowercase()
            return refusedSuffix.any { n.endsWith(it) } || n == "desktop.ini" || n == "autorun.inf"
        }
    }

    private fun name(display: String?, media: Media? = null, mimeExt: String? = null) = ShareNames.finalName(display, media, mimeExt, "Shared_1", FakeRules)

    // ---- share names ----

    @Test fun a_shared_file_keeps_its_own_extension_lowercased() {
        assertEquals("report.pdf", name("report.PDF"))
        assertEquals("archive.tar.gz", name("archive.tar.gz"))
        assertEquals("notes.docx", name("notes.docx"))
    }

    @Test fun a_file_with_no_extension_stays_without_one() {
        assertEquals("Makefile", name("Makefile"))
        assertEquals("gitignore", name(".gitignore")) // a leading dot is not an extension; the core refuses dot-names, so it is cleaned
        assertEquals("a.b c", name("a.b c")) // "b c" is not an extension the core would read
    }

    @Test fun trailing_dots_and_spaces_do_not_hide_the_extension() {
        assertEquals("a.pdf", name("a.pdf. "))
    }

    @Test fun the_mime_type_gives_an_extension_only_when_the_name_has_none() {
        assertEquals("data.csv", name("data", mimeExt = "csv"))
        assertEquals("report.pdf", name("report.pdf", mimeExt = "doc"))
        assertEquals("data", name("data", mimeExt = "a b")) // an extension the core would not read is never added
        assertEquals("Shared_1.pdf", name(null, mimeExt = "pdf"))
        assertEquals("Shared_1", name(null))
    }

    @Test fun unsafe_characters_never_reach_the_core() {
        assertEquals("a_b.pdf", name("a:b.pdf"))
        assertEquals("a_b.pdf", name("a/b.pdf"))
        assertEquals("a_b.pdf", name("a\\b.pdf"))
        assertEquals("a_b.pdf", name("a..b.pdf"))
    }

    @Test fun a_photo_or_video_takes_the_extension_of_its_bytes() {
        assertEquals("IMG.jpg", name("IMG", Media.JPEG))
        assertEquals("IMG.png", name("IMG.jpg", Media.PNG))
        assertEquals("photo.heic", name("photo.jpg", Media.HEIF))
        assertEquals("clip.mp4", name("clip", Media.MP4))
        assertEquals("Shared_1.png", name(null, Media.PNG))
    }

    @Test fun a_name_that_says_document_or_audio_keeps_its_type_whatever_the_bytes_look_like() {
        // Audio in an MP4 container sniffs as a video; the user shared a voice memo, not a video.
        assertEquals("voice.m4a", name("voice.m4a", Media.MP4))
        assertEquals("memo.3ga", name("memo.3ga", Media.THREE_GP))
        assertEquals("scan.pdf", name("scan.pdf", Media.JPEG))
        assertEquals(null, ShareNames.mediaFor("voice.m4a", Media.MP4, FakeRules))
        assertEquals(Media.MP4, ShareNames.mediaFor("clip", Media.MP4, FakeRules))
        assertEquals(Media.MP4, ShareNames.mediaFor("clip.MOV", Media.MP4, FakeRules))
        assertEquals(null, ShareNames.mediaFor("clip", null, FakeRules))
    }

    @Test fun a_photo_extension_on_other_bytes_is_left_for_the_core_to_refuse() {
        // No sniffed media: the name keeps jpg, the core's magic-byte check refuses it (a program cannot pose as a photo).
        assertEquals("virus.jpg", name("virus.jpg"))
    }

    @Test fun shortcut_and_folder_setting_names_are_refused_before_any_copy() {
        for (n in listOf("x.lnk", "X.LNK", "x.lnk. ", "link.url", "a.scf", "y.library-ms", "z.searchConnector-ms", "desktop.ini", "DESKTOP.INI", "autorun.inf")) {
            assertTrue(n, ShareNames.refused(n, FakeRules))
        }
        for (n in listOf("lnk.txt", "desktop.ini.txt", "mylnk", "report.pdf", "Makefile")) {
            assertFalse(n, ShareNames.refused(n, FakeRules))
        }
        assertFalse(ShareNames.refused(null, FakeRules))
        // The final name is checked again: a refused name cannot slip through by cleaning.
        assertTrue(ShareNames.refused(name("x.lnk"), FakeRules))
    }

    @Test fun the_refusal_line_matches_what_the_pc_says() {
        assertEquals("shortcut and folder-setting files never sync", ShareNames.REFUSED_WHY)
        val r = ShareResult(1, listOf("x.lnk (${ShareNames.REFUSED_WHY})"), "Inbox")
        assertEquals("1 added to Inbox. NOT SYNCED: x.lnk (shortcut and folder-setting files never sync)", r.summary())
        assertEquals("NOT SYNCED: x.lnk (shortcut and folder-setting files never sync)", ShareResult(0, listOf("x.lnk (${ShareNames.REFUSED_WHY})"), "Inbox").summary())
    }

    // ---- which sources are read (dirty stream) ----

    @Test fun only_content_uris_from_other_apps_are_read() {
        val own = "app.snapferry.fileprovider"
        assertFalse(ShareNames.acceptableUri("file", null, own)) // file:///data/data/app.snapferry/files/tugboat.db
        assertFalse(ShareNames.acceptableUri("file", "", own))
        assertFalse(ShareNames.acceptableUri("content", own, own)) // our own provider
        assertFalse(ShareNames.acceptableUri("content", "APP.SNAPFERRY.FILEPROVIDER", own))
        assertFalse(ShareNames.acceptableUri("content", null, own))
        assertFalse(ShareNames.acceptableUri(null, "media", own))
        assertTrue(ShareNames.acceptableUri("content", "media", own)) // content://media/external/file/12
    }

    @Test fun the_share_target_takes_any_type_and_the_provider_is_the_naming_one() {
        val m = File(main, "AndroidManifest.xml").readText()
        val share = m.substring(m.indexOf("android:name=\".ShareActivity\""), m.indexOf("<service"))
        assertTrue(share.contains("android.intent.action.SEND\""))
        assertTrue(share.contains("android.intent.action.SEND_MULTIPLE"))
        assertEquals(2, Regex("""android:mimeType="\*/\*"""").findAll(share).count())
        assertFalse("old photo-only filter still there", share.contains("image/*") || share.contains("video/*"))
        assertTrue(m.contains("""android:name=".ItemFileProvider""""))
        assertTrue(m.contains("android:exported=\"false\""))
    }

    @Test fun the_provider_still_serves_only_held_items_and_the_export_folder() {
        val xml = File(main, "res/xml/file_paths.xml").readText()
        val paths = Regex("""<(\w+-path) name="(\w+)" path="([^"]+)"""").findAll(xml).map { it.groupValues[1] to it.groupValues[3] }.toList()
        assertEquals(listOf("cache-path" to "export/", "files-path" to "storage/items/"), paths)
    }

    @Test fun a_file_is_opened_through_a_content_uri_never_a_file_uri() {
        for (n in listOf("Files.kt", "Export.kt", "SnapViewModel.kt")) { // the in-app video player reads its own file directly; nothing leaves the app that way
            val src = File(kotlin, n).readText()
            assertFalse("$n builds a file:// uri", src.contains("Uri.fromFile") || src.contains("\"file://"))
        }
        val files = File(kotlin, "Files.kt").readText()
        assertTrue(files.contains("Intent.ACTION_VIEW"))
        assertTrue(files.contains("FLAG_GRANT_READ_URI_PERMISSION"))
        assertTrue(files.contains("FileProvider.getUriForFile"))
    }

    @Test fun a_share_with_no_stream_says_nothing_to_save() {
        val src = File(kotlin, "ShareActivity.kt").readText()
        assertTrue(src.contains("NOTHING TO SAVE - share a file"))
    }

    // ---- copy with a limit ----

    @Test fun a_copy_inside_the_room_succeeds_and_counts_bytes() {
        val out = ByteArrayOutputStream()
        assertEquals(10L, Streams.copyWithin(ByteArrayInputStream(ByteArray(10) { it.toByte() }), out, 10))
        assertEquals(10, out.size())
        assertEquals(0L, Streams.copyWithin(ByteArrayInputStream(ByteArray(0)), ByteArrayOutputStream(), 0))
    }

    @Test fun a_copy_past_the_room_stops_with_phone_storage_low() {
        val out = ByteArrayOutputStream()
        try {
            Streams.copyWithin(ByteArrayInputStream(ByteArray(11)), out, 10)
            fail("expected OutOfRoom")
        } catch (e: OutOfRoom) {
            assertEquals("phone storage low", e.message)
        }
        // A negative room (free space already under the margin) refuses any content.
        try {
            Streams.copyWithin(ByteArrayInputStream(ByteArray(1)), ByteArrayOutputStream(), -5)
            fail("expected OutOfRoom")
        } catch (e: OutOfRoom) {
        }
    }

    @Test fun a_source_that_never_ends_cannot_fill_the_phone() {
        val endless = object : InputStream() {
            var served = 0L
            override fun read() = 0.also { served++ }
            override fun read(b: ByteArray, off: Int, len: Int): Int {
                served += len
                return len
            }
        }
        val room = 1_000_000L
        try {
            Streams.copyWithin(endless, ByteArrayOutputStream(), room)
            fail("expected OutOfRoom")
        } catch (e: OutOfRoom) {
        }
        assertTrue("read far past the room: ${endless.served}", endless.served <= room + 64 * 1024)
    }

    // The cache copy and the core's copy are both on the disk at the peak: a share needs twice its size.
    @Test fun a_share_must_fit_on_the_phone_twice() {
        val mb = 1024L * 1024
        val free = 2048 * mb
        val max = 16L * 1024 * mb
        // 1.5 GB into 2 GB free: one copy fits, two do not.
        assertEquals("phone storage low", ShareImporter.refusal(1536 * mb, free, max))
        // Half of what is free beyond the 64 MB margin still goes in.
        assertNull(ShareImporter.refusal((free - 64 * mb) / 2, free, max))
        assertEquals("phone storage low", ShareImporter.refusal((free - 64 * mb) / 2 + 1, free, max))
        // No declared size: nothing to refuse yet, the copy itself is capped.
        assertNull(ShareImporter.refusal(null, free, max))
        assertEquals((free - 64 * mb) / 2 - 100, ShareImporter.copyRoom(free, 100))
    }

    @Test fun a_file_over_the_core_limit_is_refused_before_the_copy() {
        val gb = 1024L * 1024 * 1024
        assertEquals("too large", ShareImporter.refusal(20 * gb, 60 * gb, 16 * gb))
        assertNull(ShareImporter.refusal(10 * gb, 60 * gb, 16 * gb))
    }

    // ---- file labels, icons, names other apps see ----

    @Test fun file_meta_reads_type_then_size() {
        assertEquals("PDF, 1.2 MB", FileRules.meta("pdf", 1_300_000L))
        assertEquals("ZIP, 850 MB", FileRules.meta("zip", 850L shl 20))
        assertEquals("File, 3 KB", FileRules.meta("", 3 * 1024L))
        assertEquals("MP4, 1.5 GB", FileRules.meta("mp4", 3L shl 29))
        assertEquals("TXT, 0 B", FileRules.meta("txt", 0))
        assertEquals("TXT, 512 B", FileRules.meta("txt", 512))
        assertEquals("ISO, 12 GB", FileRules.meta("iso", 12L shl 30))
    }

    @Test fun every_file_has_one_of_four_icons() {
        assertEquals(FileGlyph.DOCUMENT, FileRules.glyph("pdf"))
        assertEquals(FileGlyph.DOCUMENT, FileRules.glyph("DOCX"))
        assertEquals(FileGlyph.ARCHIVE, FileRules.glyph("zip"))
        assertEquals(FileGlyph.AUDIO, FileRules.glyph("mp3"))
        assertEquals(FileGlyph.OTHER, FileRules.glyph("exe"))
        assertEquals(FileGlyph.OTHER, FileRules.glyph(""))
    }

    @Test fun the_extension_is_shown_once() {
        assertEquals("report.pdf", FileRules.displayName("report", "pdf"))
        assertEquals("report.pdf", FileRules.displayName("report.pdf", "pdf"))
        assertEquals("report.PDF", FileRules.displayName("report.PDF", "pdf"))
        assertEquals("Makefile", FileRules.displayName("Makefile", ""))
    }

    @Test fun the_name_another_app_sees_is_the_real_name_and_never_a_path() {
        assertEquals("report.pdf", FileRules.providerName("report.pdf"))
        assertEquals("a_b.pdf", FileRules.providerName("a/b.pdf"))
        assertEquals("a_b.pdf", FileRules.providerName("a\\b.pdf"))
        assertEquals("a_b", FileRules.providerName("a\nb"))
        assertNull(FileRules.providerName(null))
        assertNull(FileRules.providerName("  "))
        assertNull(FileRules.providerName(".."))
        assertNull(FileRules.providerName("."))
        assertEquals(120, FileRules.providerName("x".repeat(500))!!.length)
    }

    @Test fun mime_for_open_and_share() {
        val platform: (String) -> String? = { mapOf("pdf" to "application/pdf", "docx" to "application/vnd.openxmlformats-officedocument.wordprocessingml.document")[it] }
        assertEquals("application/pdf", FileRules.mime("pdf", MediaKind.FILE, platform))
        assertEquals("application/octet-stream", FileRules.mime("", MediaKind.FILE, platform)) // no extension
        assertEquals("application/octet-stream", FileRules.mime("xyz", MediaKind.FILE, platform)) // the platform knows nothing
        assertEquals("image/jpeg", FileRules.mime("JPG", MediaKind.IMAGE, platform))
        assertEquals("video/mp4", FileRules.mime("mp4", MediaKind.VIDEO, platform))
        assertEquals("video/*", FileRules.mime("weird", MediaKind.VIDEO, platform))
        assertEquals("image/*", FileRules.mime("weird", MediaKind.IMAGE, platform))
    }

    @Test fun mime_gives_an_extension_only_for_a_real_type() {
        val platform: (String) -> String? = { mapOf("application/pdf" to "pdf", "text/csv" to "CSV")[it] }
        assertEquals("pdf", FileRules.extFromMime("application/pdf", platform))
        assertEquals("pdf", FileRules.extFromMime("application/pdf; charset=binary", platform))
        assertEquals("csv", FileRules.extFromMime("TEXT/CSV", platform))
        assertNull(FileRules.extFromMime("*/*", platform))
        assertNull(FileRules.extFromMime("image/*", platform))
        assertNull(FileRules.extFromMime("application/octet-stream", platform))
        assertNull(FileRules.extFromMime(null, platform))
        assertNull(FileRules.extFromMime("", platform))
        assertNull(FileRules.extFromMime("application/x-unknown", platform))
    }

    @Test fun the_no_app_line_names_the_type() {
        assertEquals("NO APP OPENS .XYZ FILES - use Share", FileRules.noAppText("xyz"))
        assertEquals("NO APP OPENS THIS FILE - use Share", FileRules.noAppText(""))
    }

    // ---- a refused service start ----

    @Test fun a_refused_service_start_is_reported_only_when_no_start_worked_in_time() {
        val said = mutableListOf<String>()
        var running = false
        runBlocking {
            StartRefusal.watch(CoroutineScope(Dispatchers.Default), 60, { running }, { said += it }, "refused").join()
        }
        assertEquals(listOf("refused"), said)

        said.clear()
        running = false
        val job = StartRefusal.watch(CoroutineScope(Dispatchers.Default), 200, { running }, { said += it }, "refused")
        running = true // a later start (the opened app) worked within the grace time
        runBlocking { job.join() }
        assertTrue("a banner for a start that was followed by a good one: $said", said.isEmpty())
    }

    @Test fun the_service_start_goes_through_the_grace_rule() {
        val svc = File(kotlin, "SnapService.kt").readText()
        assertTrue(svc.contains("StartRefusal.watch("))
        assertFalse("a refused start must not raise a banner at once", svc.contains("c.core.reportError(m)"))
    }

    // The Open button must use the tested predicate: a binned file outside the Bin route is not openable.
    @Test fun the_open_button_uses_the_open_rule() {
        val viewer = File(kotlin, "Viewer.kt").readText()
        val page = viewer.substringAfter("private fun FilePage(").substringBefore("/** The file is not here")
        assertTrue(page.contains("UiLogic.canOpen(item, bin) -> ActionButton"))
        assertFalse("Open must not be the unconditional last branch", page.contains("else -> ActionButton"))
    }

    // ---- recording ----

    @Test fun orientation_is_locked_only_while_recording() {
        assertEquals(android.content.pm.ActivityInfo.SCREEN_ORIENTATION_LOCKED, UiLogic.recordingOrientation(true))
        assertEquals(android.content.pm.ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, UiLogic.recordingOrientation(false))
        val cam = File(kotlin, "CameraScreen.kt").readText()
        assertTrue(cam.contains("UiLogic.recordingOrientation(true)"))
        assertTrue("the lock must be released on dispose", cam.contains("onDispose { if (isRecording) activity?.requestedOrientation = UiLogic.recordingOrientation(false) }"))
        // Source check: the release must not wait for a recomposition (none runs while the app is stopped).
        val finalize = cam.substringAfter("is VideoRecordEvent.Finalize ->").substringBefore("else -> {}")
        assertTrue("the lock must be released when the recording ends", finalize.contains("requestedOrientation = UiLogic.recordingOrientation(false)"))
        val main = File(kotlin, "MainActivity.kt").readText()
        assertTrue("a restored activity must not keep a stale lock", main.contains("if (savedInstanceState != null) requestedOrientation = UiLogic.recordingOrientation(false)"))
    }
}
