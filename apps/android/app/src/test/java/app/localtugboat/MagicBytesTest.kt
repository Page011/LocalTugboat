package app.localtugboat

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class MagicBytesTest {
    private fun bytes(vararg v: Int) = ByteArray(v.size) { v[it].toByte() }
    private fun ftyp(brand: String) = bytes(0, 0, 0, 0x18) + "ftyp".toByteArray() + brand.toByteArray(Charsets.ISO_8859_1) + ByteArray(8)

    @Test fun photos() {
        assertEquals(Media.JPEG, MagicBytes.sniff(bytes(0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10)))
        assertEquals(Media.PNG, MagicBytes.sniff(bytes(0x89, 'P'.code, 'N'.code, 'G'.code, 0x0D, 0x0A, 0x1A, 0x0A, 0)))
        assertEquals(Media.GIF, MagicBytes.sniff("GIF89a....".toByteArray()))
        assertEquals(Media.GIF, MagicBytes.sniff("GIF87a....".toByteArray()))
        assertEquals(Media.WEBP, MagicBytes.sniff("RIFF\u0001\u0002\u0003\u0004WEBPVP8 ".toByteArray(Charsets.ISO_8859_1)))
        assertEquals(Media.HEIF, MagicBytes.sniff(ftyp("heic")))
        assertEquals(Media.HEIF, MagicBytes.sniff(ftyp("mif1")))
    }

    @Test fun videos() {
        assertEquals(Media.MP4, MagicBytes.sniff(ftyp("isom")))
        assertEquals(Media.MP4, MagicBytes.sniff(ftyp("mp42")))
        assertEquals(Media.MOV, MagicBytes.sniff(ftyp("qt  ")))
        assertEquals(Media.THREE_GP, MagicBytes.sniff(ftyp("3gp4")))
        assertEquals(Media.THREE_GP, MagicBytes.sniff(ftyp("3g2a")))
        assertEquals(true, Media.MP4.video)
        assertEquals(false, Media.JPEG.video)
    }

    @Test fun things_that_are_not_media_are_refused() {
        assertNull(MagicBytes.sniff("MZ\u0090\u0000\u0003".toByteArray(Charsets.ISO_8859_1))) // a Windows program
        assertNull(MagicBytes.sniff("PK\u0003\u0004".toByteArray(Charsets.ISO_8859_1)))       // a zip
        assertNull(MagicBytes.sniff("%PDF-1.7".toByteArray()))
        assertNull(MagicBytes.sniff("[InternetShortcut]".toByteArray()))
        assertNull(MagicBytes.sniff("RIFF\u0000\u0000\u0000\u0000WAVEfmt ".toByteArray(Charsets.ISO_8859_1))) // RIFF but a WAV
        assertNull(MagicBytes.sniff(ftyp("M4A "))) // ISO-BMFF audio, not a video
        assertNull(MagicBytes.sniff(ByteArray(0)))
        assertNull(MagicBytes.sniff(bytes(0xFF, 0xD8))) // JPEG cut short
    }

    @Test fun only_the_given_length_counts() {
        // A buffer longer than what was read: the stale tail must not complete a magic number.
        val buf = bytes(0xFF, 0xD8, 0xFF, 0xE0)
        assertNull(MagicBytes.sniff(buf, 2))
        assertEquals(Media.JPEG, MagicBytes.sniff(buf, 3))
    }

    @Test fun mime_follows_the_sniffed_type() {
        assertEquals("image/jpeg", Media.JPEG.mime)
        assertEquals("video/quicktime", Media.MOV.mime)
    }
}
