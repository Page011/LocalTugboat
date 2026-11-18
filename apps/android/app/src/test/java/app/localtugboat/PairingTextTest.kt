package app.localtugboat

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PairingTextTest {
    private val good = "localtugboat://pair?v=1&k=" + "ab".repeat(32) + "&a=192.168.1.5:47300&s=" + "cd".repeat(16) + "&e=1795000000000&n=NIGEL-PC"

    @Test fun accepts_a_payload_and_trims_it() {
        assertEquals(good, PairingText.candidate(good))
        assertEquals(good, PairingText.candidate("  $good\n"))
    }

    @Test fun ignores_everything_else_a_camera_may_see() {
        assertNull(PairingText.candidate(null))
        assertNull(PairingText.candidate(""))
        assertNull(PairingText.candidate("https://example.com/pair?x=1"))
        assertNull(PairingText.candidate("localtugboat://other?x=1"))
        assertNull(PairingText.candidate(PairingText.PREFIX)) // prefix only
        assertNull(PairingText.candidate("LOCALTUGBOAT://pair?v=1")) // the intent filter is case sensitive too
    }

    @Test fun refuses_spaces_newlines_controls_and_non_ascii_inside() {
        assertNull(PairingText.candidate("$good extra"))
        assertNull(PairingText.candidate("localtugboat://pair?v=1&n=a\nb"))
        assertNull(PairingText.candidate("localtugboat://pair?v=1&n=é"))
    }

    @Test fun refuses_over_512_bytes() {
        val atLimit = "localtugboat://pair?" + "a".repeat(PairingText.MAX_LEN - PairingText.PREFIX.length)
        assertEquals(PairingText.MAX_LEN, atLimit.length)
        assertEquals(atLimit, PairingText.candidate(atLimit))
        assertNull(PairingText.candidate(atLimit + "a"))
    }

    // ---- Share names ----

    @Test fun share_stem_is_made_safe_for_the_core() {
        assertEquals("IMG_1", ShareNames.safeStem("IMG_1.jpg", "fb"))
        assertEquals("a_b", ShareNames.safeStem("a:b.jpg", "fb"))
        assertEquals("a_b", ShareNames.safeStem("a/b.jpg", "fb"))
        assertEquals("a_b", ShareNames.safeStem("a\\b.jpg", "fb"))
        assertEquals("a_b", ShareNames.safeStem("a..b.jpg", "fb")) // ".." is refused by the core
        assertEquals("x", ShareNames.safeStem("x. .jpg", "fb")) // trailing dots and spaces are stripped
        assertEquals("fb", ShareNames.safeStem(null, "fb"))
        assertEquals("fb", ShareNames.safeStem("", "fb"))
        assertEquals("_", ShareNames.safeStem("...jpg", "fb")) // stem ".." becomes "_": valid, never a path trick
        assertEquals("fb", ShareNames.safeStem(" . ", "fb"))
        assertEquals(80, ShareNames.safeStem("a".repeat(300) + ".jpg", "fb").length)
        assertEquals("tab_here", ShareNames.safeStem("tab\there.png", "fb"))
    }

    @Test fun share_extension_comes_from_the_content_not_the_name() {
        assertEquals("photo.heic", ShareNames.finalName("photo.jpg", Media.HEIF, null, "fb", AnyFileTest.FakeRules))
        assertEquals("photo.mp4", ShareNames.finalName("photo", Media.MP4, null, "fb", AnyFileTest.FakeRules))
        assertEquals("fb.png", ShareNames.finalName(null, Media.PNG, null, "fb", AnyFileTest.FakeRules))
    }

    @Test fun share_accepts_only_foreign_content_uris() {
        val own = "app.snapferry.fileprovider"
        assertTrue(ShareNames.acceptableUri("content", "media", own))
        assertTrue(ShareNames.acceptableUri("content", "com.google.android.apps.photos.contentprovider", own))
        assertFalse(ShareNames.acceptableUri("file", null, own)) // would read our own private files
        assertFalse(ShareNames.acceptableUri("content", own, own))
        assertFalse(ShareNames.acceptableUri("content", "APP.SNAPFERRY.FILEPROVIDER", own))
        assertFalse(ShareNames.acceptableUri("http", "example.com", own))
        assertFalse(ShareNames.acceptableUri(null, null, own))
    }

    @Test fun share_summary() {
        assertEquals("2 added to Inbox", ShareResult(2, emptyList(), "Inbox").summary())
        assertEquals("1 added to Work. NOT SYNCED: a.zip (not a photo or video)", ShareResult(1, listOf("a.zip (not a photo or video)"), "Work").summary())
        assertEquals("NOT SYNCED: a.zip (not a photo or video)", ShareResult(0, listOf("a.zip (not a photo or video)"), "Inbox").summary())
        assertEquals("Nothing to add", ShareResult(0, emptyList(), "Inbox").summary())
    }
}
