package app.localtugboat

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * A shared file keeps the name and type the user shared. Audio in an MP4/3GP container (voice recorder .m4a / .3ga,
 * AAC .m4a with the common "mp42" / "isom" brands) looks like a video to the sniffer but is never renamed to a video
 * extension; a real video with a wrong extension does get the sniffed one.
 */
class ShareNameSniffTest {
    private object Rules : NameRules {
        private val ok = Regex("^[a-z0-9_-]{1,16}$")
        override fun extOf(name: String): String {
            val n = name.trimEnd(' ', '.')
            val i = n.lastIndexOf('.')
            if (i <= 0) return ""
            val e = n.substring(i + 1).lowercase()
            return if (ok.matches(e)) e else ""
        }
        override fun isRefused(name: String) = false
    }

    private fun bmff(brand: String): ByteArray {
        val b = ByteArray(32)
        b[3] = 0x20
        "ftyp".forEachIndexed { i, c -> b[4 + i] = c.code.toByte() }
        brand.forEachIndexed { i, c -> b[8 + i] = c.code.toByte() }
        return b
    }

    private fun shared(display: String, head: ByteArray): String =
        ShareNames.finalName(display, MagicBytes.sniff(head), null, "Shared_1", Rules)

    @Test
    fun an_m4a_audio_file_keeps_its_name() {
        assertEquals("voice.m4a", shared("voice.m4a", bmff("mp42")))
        assertEquals("song.m4a", shared("song.m4a", bmff("isom")))
    }

    @Test
    fun a_3ga_voice_recording_keeps_its_name() {
        assertEquals("memo.3ga", shared("memo.3ga", bmff("3gp4")))
    }

    @Test
    fun a_video_with_a_wrong_extension_gets_the_sniffed_one() {
        assertEquals("clip.mp4", shared("clip.mp4", bmff("isom")))
    }
}
