package app.localtugboat

import android.content.ContentResolver
import android.content.Intent
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import androidx.core.content.FileProvider
import java.io.File
import java.io.IOException
import uniffi.tugboat_ffi.ItemInfo
import uniffi.tugboat_ffi.MediaKind

/** Which icon a file tile wears. Four are enough: the extension is written next to it. */
enum class FileGlyph { DOCUMENT, ARCHIVE, AUDIO, OTHER }

/**
 * Pure rules for files that are not photos or videos: icon, labels, names shown to other apps, MIME types.
 * No Android classes (the platform MIME lookup is passed in), so every rule has a JVM test.
 */
object FileRules {
    private val DOCUMENT = setOf("pdf", "doc", "docx", "odt", "rtf", "txt", "md", "csv", "xls", "xlsx", "ods", "ppt", "pptx", "odp", "json", "xml", "html", "log")
    private val ARCHIVE = setOf("zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz")
    private val AUDIO = setOf("mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "amr")

    fun glyph(ext: String): FileGlyph = when (ext.lowercase()) {
        in DOCUMENT -> FileGlyph.DOCUMENT
        in ARCHIVE -> FileGlyph.ARCHIVE
        in AUDIO -> FileGlyph.AUDIO
        else -> FileGlyph.OTHER
    }

    /** "512 B", "3 KB", "1.2 MB", "12 MB", "1.5 GB": one decimal under 10, none above, so a file tile shows a real size. */
    fun sizeText(n: Long): String {
        fun unit(v: Double, u: String) = if (v < 10) "%.1f %s".format(java.util.Locale.ROOT, v, u) else "${Math.round(v)} $u"
        return when {
            n >= 1L shl 30 -> unit(n / (1L shl 30).toDouble(), "GB")
            n >= 1L shl 20 -> unit(n / (1L shl 20).toDouble(), "MB")
            n >= 1L shl 10 -> "${n shr 10} KB"
            else -> "${n.coerceAtLeast(0)} B"
        }
    }

    /** "PDF, 1.2 MB". A file with no extension reads "File, 3 KB". */
    fun meta(ext: String, sizeBytes: Long): String = "${if (ext.isEmpty()) "File" else ext.uppercase()}, ${sizeText(sizeBytes)}"

    /** The name with its extension, once: core item names already carry it, but never trust that for a display. */
    fun displayName(name: String, ext: String): String =
        if (ext.isEmpty() || name.endsWith(".$ext", ignoreCase = true)) name else "$name.$ext"

    /**
     * The name another app sees for a shared or opened file. Path separators and control characters go (a name
     * is shown, never used as a path), the length is capped. Null = nothing usable: the provider keeps its own name.
     */
    fun providerName(raw: String?): String? {
        // Direction controls (U+061C, U+200E/F, U+202A-E, U+2066-9) go too: "invoice<RLO>fdp.exe" reads "invoiceexe.pdf".
        val s = raw?.replace(Regex("""[\u061C\u200E\u200F\u202A-\u202E\u2066-\u2069]"""), "")?.replace(Regex("""[\\/\p{Cntrl}]"""), "_")?.trim()?.take(120)?.trim()
        return if (s.isNullOrEmpty() || s == "." || s == "..") null else s
    }

    /** MIME for an opening or sharing intent. Photos and videos keep their fixed table; anything else asks the platform. */
    fun mime(ext: String, kind: MediaKind, platform: (String) -> String?): String {
        val e = ext.lowercase()
        MEDIA_MIME[e]?.let { return it }
        if (kind == MediaKind.VIDEO) return "video/*"
        if (kind == MediaKind.IMAGE) return "image/*"
        return (if (e.isEmpty()) null else platform(e)) ?: "application/octet-stream"
    }

    private val MEDIA_MIME = mapOf(
        "jpg" to "image/jpeg", "jpeg" to "image/jpeg", "png" to "image/png", "gif" to "image/gif", "webp" to "image/webp",
        "heic" to "image/heic", "heif" to "image/heic", "mp4" to "video/mp4", "m4v" to "video/mp4", "mov" to "video/quicktime", "3gp" to "video/3gpp",
    )

    /** An extension for a shared file that names none, from its MIME type. Null for the types that say nothing. */
    fun extFromMime(mime: String?, platform: (String) -> String?): String? {
        val m = mime?.substringBefore(';')?.trim()?.lowercase() ?: return null
        if (m.isEmpty() || m.endsWith("/*") || m == "application/octet-stream") return null
        return platform(m)?.lowercase()?.takeIf { it.isNotEmpty() }
    }

    /** The "no app" line. */
    fun noAppText(ext: String) = if (ext.isEmpty()) "NO APP OPENS THIS FILE - use Share" else "NO APP OPENS .${ext.uppercase()} FILES - use Share"
}

/** Platform MIME lookups, passed to [FileRules]. */
object PlatformMime {
    val ofExt: (String) -> String? = { MimeTypeMap.getSingleton().getMimeTypeFromExtension(it) }
    val extOf: (String) -> String? = { MimeTypeMap.getSingleton().getExtensionFromMimeType(it) }
}

/**
 * FileProvider that answers a content query with the item's real file name. Held files are stored as
 * `<uuid>.<ext>`, so without this the app that opens or receives a share would see the uuid. The name rides in
 * the URI's query (`?n=`), which FileProvider ignores when it finds the file; it is display only.
 */
class ItemFileProvider : FileProvider() {
    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
        val base = super.query(uri, projection, selection, selectionArgs, sortOrder)
        val name = FileRules.providerName(uri.getQueryParameter(NAME_PARAM)) ?: return base
        try {
            val cols = base.columnNames
            val out = MatrixCursor(cols, 1)
            if (base.moveToFirst()) {
                val row = out.newRow()
                for (i in cols.indices) {
                    row.add(
                        when {
                            cols[i] == OpenableColumns.DISPLAY_NAME -> name
                            base.getType(i) == Cursor.FIELD_TYPE_INTEGER -> base.getLong(i)
                            base.getType(i) == Cursor.FIELD_TYPE_NULL -> null
                            else -> base.getString(i)
                        },
                    )
                }
            }
            return out
        } finally {
            base.close()
        }
    }

    companion object {
        const val NAME_PARAM = "n"

        fun authority(c: android.content.Context) = "${c.packageName}.fileprovider"

        /** content:// URI for a held item with its real name attached. Throws IOException for a file outside items/ (the Bin). */
        fun uriFor(c: android.content.Context, item: ItemInfo): Uri {
            val path = item.path ?: throw IOException("File is not on this phone yet")
            val plain = try {
                FileProvider.getUriForFile(c, authority(c), File(path))
            } catch (e: IllegalArgumentException) {
                throw IOException("This file cannot be opened or shared from the Bin. Restore it first")
            }
            return plain.buildUpon().appendQueryParameter(NAME_PARAM, FileRules.displayName(item.name, item.ext)).build()
        }

        /** ACTION_VIEW for the file: a content URI with a read grant, never file://. The platform picks the default app. */
        fun openIntent(c: android.content.Context, item: ItemInfo): Intent {
            val uri = uriFor(c, item)
            return Intent(Intent.ACTION_VIEW)
                .setDataAndType(uri, FileRules.mime(item.ext, item.kind, PlatformMime.ofExt))
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        }
    }
}

/** Content resolver helpers for Share > Tugboat. */
object ShareQuery {
    fun type(r: ContentResolver, uri: Uri): String? = try {
        r.getType(uri)
    } catch (e: Exception) {
        null
    }
}
