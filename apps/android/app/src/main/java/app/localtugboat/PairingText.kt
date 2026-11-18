package app.localtugboat

/**
 * Cheap pre-check of text that might be a pairing payload (scanned QR, deep link, pasted text).
 * It only decides "is this worth asking the core about"; the core's inspectPairingPayload is the
 * real validation (fields, local-only addresses, expiry). Anything that fails here is ignored
 * silently by the scanner, because a camera sees many unrelated QR codes.
 */
object PairingText {
    const val PREFIX = "localtugboat://pair?"

    /** Mirrors the core limit: payload is ASCII, one line, at most 512 bytes. */
    const val MAX_LEN = 512

    /** The trimmed payload, or null when the text cannot be a Tugboat pairing payload. */
    fun candidate(raw: String?): String? {
        val s = raw?.trim() ?: return null
        if (s.length > MAX_LEN || !s.startsWith(PREFIX) || s.length == PREFIX.length) return null
        // Printable ASCII only: no spaces, newlines or control characters inside.
        if (!s.all { it.code in 0x21..0x7E }) return null
        return s
    }
}

/**
 * The two naming rules the core owns, behind an interface so JVM tests can stand in for the native library.
 * The app never keeps its own copy of the rules: [CoreNameRules] asks the core (the same code the PC runs).
 */
interface NameRules {
    /** The extension core would give this name: lowercase, or "" for none. */
    fun extOf(name: String): String
    /** True for names that never sync (shortcuts, desktop.ini, autorun.inf, ...). */
    fun isRefused(name: String): Boolean
}

object CoreNameRules : NameRules {
    override fun extOf(name: String) = uniffi.tugboat_ffi.namesExtOf(name)
    override fun isRefused(name: String) = uniffi.tugboat_ffi.namesIsRefused(name)
}

/** Names for files that come from other apps (Share > Tugboat). */
object ShareNames {
    private const val MAX_STEM = 80
    private val BAD = Regex("""[\\/:*?"<>|\p{Cntrl}]""")

    /** Why a shared file is refused before any byte is copied. The core says the same on the PC. */
    const val REFUSED_WHY = "shortcut and folder-setting files never sync"

    /** The same, short enough for a toast. */
    const val REFUSED_SHORT = "shortcut file"

    /** A stem the core will accept: no path separators, ':' or reserved characters, no "..", not empty. */
    fun safeStem(displayName: String?, fallback: String): String =
        clean(displayName?.substringBeforeLast('.', displayName)?.trim().orEmpty(), fallback)

    private fun clean(base: String, fallback: String): String {
        val cleaned = BAD.replace(base, "_").replace("..", "_").trim(' ', '.').take(MAX_STEM).trim(' ', '.')
        return cleaned.ifEmpty { fallback }
    }

    /**
     * Only content:// URIs a sender could really grant. file:// would let a hostile app make us read our own
     * private files (we run with our own rights), and our own provider is never a legitimate source.
     */
    fun acceptableUri(scheme: String?, authority: String?, ownAuthority: String): Boolean =
        // ContentResolver drops a "<userId>@" prefix before it finds the provider, so "0@<ours>" IS ours.
        scheme == "content" && authority != null && !authority.substringAfterLast('@').equals(ownAuthority, ignoreCase = true)

    /** True when this name must never sync. The core's answer is final; this only refuses early, before a large copy. */
    fun refused(name: String?, rules: NameRules = CoreNameRules): Boolean = name != null && rules.isRefused(name)

    private val MEDIA_EXTS = setOf("jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "mp4", "m4v", "mov", "3gp")

    /**
     * The sniffed photo/video type to act on, or null for a plain file. The bytes alone do not make a photo or
     * video: audio in an MP4/3GP container (.m4a, .3ga) looks like a video. The sniffed type counts only when the
     * display name has no extension or already carries a photo/video one.
     */
    fun mediaFor(displayName: String?, sniffed: Media?, rules: NameRules = CoreNameRules): Media? {
        if (sniffed == null) return null
        val shown = displayName?.trimEnd(' ', '.').orEmpty()
        val ext = if (shown.isEmpty()) "" else rules.extOf(shown)
        return if (ext.isEmpty() || ext in MEDIA_EXTS) sniffed else null
    }

    /**
     * Final name for a shared file.
     *  - A photo or video (known by its first bytes, [media], and not an audio file in a video container, see
     *    [mediaFor]): cleaned stem plus the SNIFFED extension, so name and content always agree.
     *  - Anything else: the cleaned stem plus the display name's own extension (as the core reads it), else the one
     *    the MIME type implies ([mimeExt]), else none ("Makefile" stays "Makefile").
     * The core re-checks every name; a photo extension on non-photo bytes is refused there.
     */
    fun finalName(displayName: String?, media: Media?, mimeExt: String?, fallback: String, rules: NameRules = CoreNameRules): String {
        val m = mediaFor(displayName, media, rules)
        if (m != null) return "${safeStem(displayName, fallback)}.${m.ext}"
        val shown = displayName?.trimEnd(' ', '.').orEmpty()
        var ext = if (shown.isEmpty()) "" else rules.extOf(shown)
        val base = if (ext.isNotEmpty() && shown.endsWith(".$ext", ignoreCase = true)) shown.dropLast(ext.length + 1) else { ext = ""; shown }
        if (ext.isEmpty() && mimeExt != null && rules.extOf("x.$mimeExt") == mimeExt) ext = mimeExt
        val stem = clean(base, fallback)
        return if (ext.isEmpty()) stem else "$stem.$ext"
    }
}
