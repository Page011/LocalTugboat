package app.localtugboat

import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import androidx.core.content.FileProvider
import java.io.File
import java.io.IOException
import uniffi.tugboat_ffi.ItemInfo

private fun authority(c: Context) = "${c.packageName}.fileprovider"

/** "Export log": one text file (Rust log, rotated files oldest first, then the Kotlin log) for the share sheet. */
object LogExport {
    /** Blocking. Works even when the core cannot open: the file then says why. */
    fun intent(c: Context): Intent {
        val dir = File(c.cacheDir, "export").apply { mkdirs() }
        dir.listFiles()?.forEach { it.delete() }
        val out = File(dir, "tugboat-log.txt")
        val rustLog = try {
            File(c.core.phone().logPath())
        } catch (e: Exception) {
            out.appendText("core not open: ${e.message}\n")
            null
        }
        out.appendText("Tugboat ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}), Android ${Build.VERSION.SDK_INT}, ${Build.MODEL}\n")
        val parts = buildList {
            rustLog?.let { addAll(listOf(File("${it.path}.2"), File("${it.path}.1"), it)) }
            add(KLog.file(c))
            add(File(c.filesDir, LegacyMigration.NOTE)) // what the 0.1.0 -> 0.1.1 upgrade moved or could not move
        }
        for (f in parts.filter { it.exists() }) {
            out.appendText("\n==== ${f.name} ====\n")
            f.inputStream().use { i -> out.appendBytes(i.readBytes()) }
        }
        val uri = FileProvider.getUriForFile(c, authority(c), out)
        return Intent.createChooser(
            Intent(Intent.ACTION_SEND).setType("text/plain").putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
            "Export log",
        )
    }
}

/** Share and "Save to phone gallery" for one item. */
object ItemExport {
    /** Share sheet for the held file. Throws IOException with a readable message when the file is not on this phone. */
    fun shareIntent(c: Context, item: ItemInfo): Intent {
        val path = item.path ?: throw IOException("File is not on this phone yet")
        val uri = try {
            FileProvider.getUriForFile(c, authority(c), File(path))
        } catch (e: IllegalArgumentException) {
            throw IOException("This file cannot be shared from the Bin. Restore it first")
        }
        return Intent.createChooser(
            Intent(Intent.ACTION_SEND).setType(mimeOf(item)).putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
            "Share",
        )
    }

    /**
     * Copy into Pictures/Tugboat or Movies/Tugboat through MediaStore (no storage permission needed).
     * The copy leaves Tugboat's control: it never expires. Blocking. Throws IOException.
     */
    fun saveToGallery(c: Context, item: ItemInfo): Uri {
        val src = File(item.path ?: throw IOException("File is not on this phone yet"))
        val video = item.kind == uniffi.tugboat_ffi.MediaKind.VIDEO
        val collection = if (video) MediaStore.Video.Media.EXTERNAL_CONTENT_URI else MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        val name = if (item.name.endsWith(".${item.ext}", ignoreCase = true)) item.name else "${item.name}.${item.ext}"
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, name)
            put(MediaStore.MediaColumns.MIME_TYPE, mimeOf(item))
            put(MediaStore.MediaColumns.RELATIVE_PATH, (if (video) Environment.DIRECTORY_MOVIES else Environment.DIRECTORY_PICTURES) + "/Tugboat")
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }
        val resolver = c.contentResolver
        val uri = resolver.insert(collection, values) ?: throw IOException("The phone gallery refused the file")
        try {
            val out = resolver.openOutputStream(uri) ?: throw IOException("The phone gallery refused the file")
            out.use { o -> src.inputStream().use { it.copyTo(o) } }
            resolver.update(uri, ContentValues().apply { put(MediaStore.MediaColumns.IS_PENDING, 0) }, null, null)
            return uri
        } catch (e: Exception) {
            // Never leave a half-written pending row in the gallery.
            resolver.delete(uri, null, null)
            throw if (e is IOException) e else IOException(e.message ?: "Save failed", e)
        }
    }

    private fun mimeOf(item: ItemInfo): String = when (item.ext.lowercase()) {
        "jpg", "jpeg" -> "image/jpeg"
        "png" -> "image/png"
        "gif" -> "image/gif"
        "webp" -> "image/webp"
        "heic", "heif" -> "image/heic"
        "mp4", "m4v" -> "video/mp4"
        "mov" -> "video/quicktime"
        "3gp" -> "video/3gpp"
        else -> if (item.kind == uniffi.tugboat_ffi.MediaKind.VIDEO) "video/*" else "image/*"
    }
}
