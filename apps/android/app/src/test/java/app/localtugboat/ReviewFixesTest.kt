package app.localtugboat

import androidx.exifinterface.media.ExifInterface
import java.io.File
import java.nio.file.Files
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.tugboat_ffi.GeoFix

/** Fixes from the PR 1 review that have a pure part. */
class ReviewFixesTest {
    // ---- shot meta (a failed hand-off keeps what the shot was meant for) ----

    @Test fun shot_meta_round_trips_with_a_place() {
        val m = ShotMeta.decode(ShotMeta("folder-1", 1_700_000_123_456L, GeoFix(-30.5123, 151.6543, 12.5)).encode())
        assertNotNull(m)
        assertEquals("folder-1", m!!.folderId)
        assertEquals(1_700_000_123_456L, m.atMs)
        assertEquals(-30.5123, m.geo!!.lat, 0.0)
        assertEquals(151.6543, m.geo!!.lon, 0.0)
        assertEquals(12.5, m.geo!!.accuracyM, 0.0)
    }

    @Test fun shot_meta_round_trips_without_a_place_and_without_a_folder() {
        val m = ShotMeta.decode(ShotMeta(null, 42L, null).encode())!!
        assertNull(m.folderId)
        assertEquals(42L, m.atMs)
        assertNull(m.geo)
    }

    @Test fun a_missing_or_corrupt_meta_is_null_never_a_crash() {
        val backslashU = "\\" + "u" // a malformed unicode escape: java.util.Properties throws on it
        assertNull(ShotMeta.decode(null))
        assertNull(ShotMeta.decode(""))
        assertNull(ShotMeta.decode("folderId=x")) // no time
        assertNull(ShotMeta.decode("atMs=yesterday"))
        assertNull(ShotMeta.decode("atMs=5\n${backslashU}ZZZZ=1"))
        assertNull(ShotMeta.decode("${backslashU}ZZZZ"))
        assertNull(ShotMeta.decode("garbage \t without any equals sign"))
    }

    @Test fun a_half_written_place_is_dropped_but_folder_and_time_stay() {
        val m = ShotMeta.decode("atMs=7\nfolderId=f\nlat=1.0\n")!!
        assertEquals("f", m.folderId)
        assertEquals(7L, m.atMs)
        assertNull(m.geo)
        assertNull(ShotMeta.decode("atMs=7\nlat=NaN\nlon=1\naccuracyM=1")!!.geo)
    }

    @Test fun the_meta_file_sits_in_a_subfolder_so_recover_does_not_take_it_for_a_shot() {
        val dir = Files.createTempDirectory("tugboat-shots").toFile()
        try {
            val shot = File(dir, "IMG_1.jpg")
            val meta = ShotMeta.file(dir, shot)
            assertEquals(File(File(dir, "meta"), "IMG_1.jpg.properties"), meta)
            assertFalse(meta.parentFile == dir)
        } finally {
            dir.deleteRecursively()
        }
    }

    // ---- EXIF orientation: mirrored photos ----

    @Test fun orientation_covers_all_eight_values_and_unknown_is_upright() {
        assertEquals(0f to false, Thumbs.orientation(ExifInterface.ORIENTATION_NORMAL))
        assertEquals(0f to true, Thumbs.orientation(ExifInterface.ORIENTATION_FLIP_HORIZONTAL))
        assertEquals(180f to false, Thumbs.orientation(ExifInterface.ORIENTATION_ROTATE_180))
        assertEquals(180f to true, Thumbs.orientation(ExifInterface.ORIENTATION_FLIP_VERTICAL))
        assertEquals(270f to true, Thumbs.orientation(ExifInterface.ORIENTATION_TRANSPOSE))
        assertEquals(90f to false, Thumbs.orientation(ExifInterface.ORIENTATION_ROTATE_90))
        assertEquals(90f to true, Thumbs.orientation(ExifInterface.ORIENTATION_TRANSVERSE))
        assertEquals(270f to false, Thumbs.orientation(ExifInterface.ORIENTATION_ROTATE_270))
        assertEquals(0f to false, Thumbs.orientation(99))
        assertEquals(0f to false, Thumbs.orientation(0))
    }

    // ---- share import: process death ----

    @Test fun an_import_token_is_interrupted_only_when_this_process_never_started_it() {
        val mine = ShareRuns.begin()
        assertFalse(ShareRuns.interrupted(mine)) // same process: keeps waiting for the running import
        assertTrue(ShareRuns.interrupted("token-from-a-dead-process"))
        assertFalse(ShareRuns.interrupted(null)) // nothing was started
    }

    // ---- R1: meta is written at the shutter, orphans are swept ----

    @Test fun sweep_removes_an_old_orphan_meta_and_keeps_the_rest() {
        val dir = Files.createTempDirectory("shots").toFile()
        try {
            val meta = File(dir, "meta").apply { mkdirs() }
            val now = 10_000_000L
            fun meta(name: String, ageMs: Long) = File(meta, "$name.properties").apply {
                writeText(ShotMeta("inbox", 1L, null).encode())
                setLastModified(now - ageMs)
            }
            val orphanOld = meta("IMG_old.jpg", 120_000)
            val orphanFresh = meta("IMG_fresh.jpg", 5_000)
            val withShot = meta("IMG_have.jpg", 120_000)
            File(dir, "IMG_have.jpg").writeText("x")
            assertEquals(1, Capture.sweepOrphanMetas(dir, now, 60_000))
            assertFalse(orphanOld.exists())
            assertTrue(orphanFresh.exists()) // the camera may still be writing its shot
            assertTrue(withShot.exists()) // its shot is there for recover()
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test fun a_camerax_temp_file_is_not_a_shot() {
        assertTrue(Capture.isCameraTemp("CameraX1234567890.tmp"))
        assertFalse(Capture.isCameraTemp("IMG_20261118_101500_000.jpg"))
        assertFalse(Capture.isCameraTemp("VID_20261118_101500_000.mp4"))
    }

    @Test fun sweep_with_no_meta_folder_is_a_no_op() {
        val dir = Files.createTempDirectory("shots").toFile()
        try {
            assertEquals(0, Capture.sweepOrphanMetas(dir, 1L, 0L))
        } finally {
            dir.deleteRecursively()
        }
    }
}
