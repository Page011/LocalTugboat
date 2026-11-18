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

/** Names for files that come from other apps (Share > Tugboat). */
object ShareNames {
    private const val MAX_STEM = 80
    private val BAD = Regex("""[\\/:*?"<>|\p{Cntrl}]""")

    /** A stem the core will accept: no path separators, ':' or reserved characters, no "..", not empty. */
    fun safeStem(displayName: String?, fallback: String): String {
        val base = displayName?.substringBeforeLast('.', displayName)?.trim().orEmpty()
        val cleaned = BAD.replace(base, "_").replace("..", "_").trim(' ', '.').take(MAX_STEM).trim(' ', '.')
        return cleaned.ifEmpty { fallback }
    }

    /**
     * Only content:// URIs a sender could really grant. file:// would let a hostile app make us read our own
     * private files (we run with our own rights), and our own provider is never a legitimate source.
     */
    fun acceptableUri(scheme: String?, authority: String?, ownAuthority: String): Boolean =
        scheme == "content" && authority != null && !authority.equals(ownAuthority, ignoreCase = true)

    /** Final name: cleaned stem plus the extension of the SNIFFED type, so name and content always agree. */
    fun finalName(displayName: String?, media: Media, fallback: String): String =
        "${safeStem(displayName, fallback)}.${media.ext}"
}
