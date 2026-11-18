package app.localtugboat

/**
 * Photo/video type from the first bytes of a file, never from a claimed MIME type or extension.
 * Mirrors tugboat-core `names::sniff` so the app refuses the same files the core would, but at
 * once, before a large copy. Core still re-checks; this is the early, friendly refusal.
 */
enum class Media(val ext: String, val video: Boolean) {
    JPEG("jpg", false), PNG("png", false), GIF("gif", false), WEBP("webp", false), HEIF("heic", false),
    MP4("mp4", true), MOV("mov", true), THREE_GP("3gp", true);

    val mime: String get() = when (this) {
        JPEG -> "image/jpeg"; PNG -> "image/png"; GIF -> "image/gif"; WEBP -> "image/webp"; HEIF -> "image/heic"
        MP4 -> "video/mp4"; MOV -> "video/quicktime"; THREE_GP -> "video/3gpp"
    }
}

object MagicBytes {
    /** Bytes `sniff` needs. */
    const val HEAD_LEN = 32

    private val HEIF_BRANDS = setOf("heic", "heix", "hevc", "hevx", "heim", "heis", "hevm", "hevs", "mif1", "msf1")
    private val MP4_BRANDS = setOf("isom", "iso2", "iso4", "iso5", "iso6", "mp41", "mp42", "avc1", "M4V ", "dash", "mmp4", "MSNV")

    /** Null = not a supported photo or video. Short input is fine: it just may not match. */
    fun sniff(head: ByteArray, len: Int = head.size): Media? {
        val n = minOf(len, head.size)
        fun at(i: Int, s: String) = n >= i + s.length && s.indices.all { head[i + it] == s[it].code.toByte() }
        fun b(i: Int) = head[i].toInt() and 0xFF
        if (n >= 3 && b(0) == 0xFF && b(1) == 0xD8 && b(2) == 0xFF) return Media.JPEG
        if (n >= 8 && b(0) == 0x89 && at(1, "PNG") && b(4) == 0x0D && b(5) == 0x0A && b(6) == 0x1A && b(7) == 0x0A) return Media.PNG
        if (at(0, "GIF87a") || at(0, "GIF89a")) return Media.GIF
        if (at(0, "RIFF") && at(8, "WEBP")) return Media.WEBP
        if (n >= 12 && at(4, "ftyp")) {
            val brand = String(head, 8, 4, Charsets.ISO_8859_1)
            return when {
                brand in HEIF_BRANDS -> Media.HEIF
                brand == "qt  " -> Media.MOV
                brand in MP4_BRANDS -> Media.MP4
                brand.startsWith("3gp") || brand.startsWith("3g2") -> Media.THREE_GP
                else -> null
            }
        }
        return null
    }
}
