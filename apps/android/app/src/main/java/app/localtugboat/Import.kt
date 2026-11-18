package app.localtugboat

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import android.media.MediaMetadataRetriever
import android.net.Uri
import android.provider.OpenableColumns
import androidx.exifinterface.media.ExifInterface
import java.io.File
import java.io.IOException
import java.util.UUID
import uniffi.tugboat_ffi.FolderKind
import uniffi.tugboat_ffi.GeoFix
import uniffi.tugboat_ffi.LocationFix
import uniffi.tugboat_ffi.NewFile
import uniffi.tugboat_ffi.CaptureRoute

/** JPEG thumbnail, longest side 512 px, made by this device (the core stores and syncs it). */
object Thumbs {
    private const val MAX_SIDE = 512

    /** Null (and a log line) when no thumbnail can be made: the item still syncs, the gallery shows a placeholder. */
    fun make(c: Context, src: File, video: Boolean, out: File): File? = try {
        val bmp = if (video) videoFrame(src) else imageBitmap(src)
        if (bmp == null) {
            KLog.log(c, "no thumbnail for ${src.name}")
            null
        } else {
            out.outputStream().use { bmp.compress(Bitmap.CompressFormat.JPEG, 80, it) }
            bmp.recycle()
            out
        }
    } catch (e: Exception) {
        KLog.log(c, "thumbnail failed for ${src.name}: $e")
        null
    }

    /** Decoded, EXIF-rotated, longest side at most [maxSide] (the viewer asks for more than a thumbnail). Blocking. */
    fun imageBitmap(src: File, maxSide: Int = MAX_SIDE): Bitmap? {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(src.path, bounds)
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        var sample = 1
        while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= maxSide) sample *= 2
        val raw = BitmapFactory.decodeFile(src.path, BitmapFactory.Options().apply { inSampleSize = sample }) ?: return null
        val rotation = when (ExifInterface(src.path).getAttributeInt(ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL)) {
            ExifInterface.ORIENTATION_ROTATE_90 -> 90f
            ExifInterface.ORIENTATION_ROTATE_180 -> 180f
            ExifInterface.ORIENTATION_ROTATE_270 -> 270f
            else -> 0f
        }
        return fit(raw, rotation, maxSide)
    }

    private fun videoFrame(src: File): Bitmap? {
        val r = MediaMetadataRetriever()
        try {
            r.setDataSource(src.path)
            val frame = r.getFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST_SYNC) ?: return null
            val rotation = r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_ROTATION)?.toFloatOrNull() ?: 0f
            return fit(frame, rotation)
        } finally {
            r.release()
        }
    }

    private fun fit(raw: Bitmap, rotation: Float, maxSide: Int = MAX_SIDE): Bitmap {
        val scale = minOf(1f, maxSide.toFloat() / maxOf(raw.width, raw.height))
        if (scale == 1f && rotation == 0f) return raw
        val m = Matrix().apply { postScale(scale, scale); postRotate(rotation) }
        val out = Bitmap.createBitmap(raw, 0, 0, raw.width, raw.height, m, true)
        if (out !== raw) raw.recycle()
        return out
    }
}

/** Where the next shot goes, for the viewfinder chip and for the shutter. */
data class Target(val folderId: String, val folderName: String, val noLocation: Boolean)

object Capture {
    /**
     * Folder for the next shot or import. Order: the folder picked on the chip, else the place rule
     * (smallest radius wins, done by the core), else Unsorted when places exist but there is no usable
     * fix, else Inbox. Blocking (SQLite). Throws when the core cannot open or has no Inbox.
     */
    fun target(c: Context, fix: LocationFix?): Target {
        val p = c.core.phone()
        val folders = p.folders()
        c.core.nextShotFolder.value?.let { id ->
            folders.firstOrNull { it.id == id }?.let { return Target(it.id, it.name, false) }
        }
        fun byKind(k: FolderKind) = folders.firstOrNull { it.kind == k } ?: throw IOException("Folder $k is missing")
        return when (val r = p.routeCapture(fix)) {
            is CaptureRoute.Place -> folders.firstOrNull { it.id == r.folderId }?.let { Target(it.id, it.name, false) }
                ?: throw IOException("Place folder is missing")
            CaptureRoute.Unsorted -> byKind(FolderKind.UNSORTED).let { Target(it.id, it.name, true) }
            CaptureRoute.Default -> byKind(FolderKind.INBOX).let { Target(it.id, it.name, false) }
        }
    }

    /**
     * Folder for a Share > Tugboat import: the folder picked on the chip, else Inbox. Place rules never
     * apply: other apps' files carry no reliable location, so they must not land in a place folder by guesswork.
     */
    fun importTarget(c: Context): Target {
        val folders = c.core.phone().folders()
        val picked = c.core.nextShotFolder.value?.let { id -> folders.firstOrNull { it.id == id } }
        val f = picked ?: folders.firstOrNull { it.kind == FolderKind.INBOX } ?: throw IOException("Inbox is missing")
        return Target(f.id, f.name, false)
    }

    /** The camera calls this after a shot is taken so the chip returns to the rule-based folder. */
    fun shotTaken(c: Context) {
        c.core.nextShotFolder.value = null
    }

    /** Camera files live here (not in cache): a shot the core could not take yet must survive until [recover] retries it. */
    fun shotsDir(c: Context): File = File(c.filesDir, "shots").apply { mkdirs() }

    /**
     * Hand a finished camera file or an import to the core. Returns the new item id. Blocking; throws
     * SnapException / IOException. [src] is deleted only when the core took it: after a failure the file
     * stays, so a photo is never lost to a transient error (the screen says so, [recover] retries).
     */
    fun add(c: Context, src: File, name: String, folderId: String?, capturedAtMs: Long, geo: GeoFix?, imported: Boolean, video: Boolean, thumbnail: Boolean = true): String {
        // One hand-off per file: [recover] must not take a file a slow hand-off is still working on.
        if (!inFlight.add(src.path)) throw IOException("${src.name} is already being added")
        try {
            return addNow(c, src, name, folderId, capturedAtMs, geo, imported, video, thumbnail)
        } finally {
            inFlight.remove(src.path)
        }
    }

    private val inFlight: MutableSet<String> = java.util.concurrent.ConcurrentHashMap.newKeySet()

    private fun addNow(c: Context, src: File, name: String, folderId: String?, capturedAtMs: Long, geo: GeoFix?, imported: Boolean, video: Boolean, thumbnail: Boolean): String {
        // Only photos and videos get a thumbnail: any other file is a tile with an icon.
        val thumbDir = File(c.cacheDir, "thumbs").apply { mkdirs() }
        val thumb = if (thumbnail) Thumbs.make(c, src, video, File(thumbDir, "${UUID.randomUUID()}.jpg")) else null
        try {
            val id = c.core.phone().addFile(
                NewFile(
                    srcPath = src.path,
                    name = name,
                    folderId = folderId,
                    capturedAtMs = capturedAtMs,
                    geo = geo,
                    thumbPath = thumb?.path,
                    imported = imported,
                )
            )
            src.delete()
            return id
        } finally {
            thumb?.delete()
        }
    }

    /**
     * Retry shots left in [shotsDir] by a crash or a failed hand-off. Files younger than [minAgeMs] are
     * skipped (a shot being written right now). Blocking. Returns how many were taken; failures stay in
     * the folder and are logged.
     */
    fun recover(c: Context, nowMs: Long = System.currentTimeMillis(), minAgeMs: Long = 60_000): Int {
        var taken = 0
        for (f in shotsDir(c).listFiles().orEmpty()) {
            if (!f.isFile || nowMs - f.lastModified() < minAgeMs || f.path in inFlight) continue
            val video = f.extension.equals("mp4", ignoreCase = true)
            try {
                if (f.length() == 0L) {
                    f.delete() // an empty file is no shot
                    continue
                }
                add(c, f, f.name, null, f.lastModified(), null, imported = false, video = video)
                taken++
            } catch (e: Exception) {
                c.core.reportError("recover ${f.name}: ${e.message}")
            }
        }
        return taken
    }
}

/** What a Share > Tugboat run did, in the words the user reads. */
class ShareResult(val added: Int, val refused: List<String>, val folderName: String) {
    /** One line that fits a toast: "2 added to Inbox" or "1 added to Inbox. NOT SYNCED: x.lnk (shortcut file)". The full reason is kept in the error list. */
    fun summary(): String {
        val ok = if (added > 0) "$added added to $folderName" else ""
        val bad = if (refused.isNotEmpty()) "NOT SYNCED: ${refused.joinToString("; ")}" else ""
        return listOf(ok, bad).filter { it.isNotEmpty() }.joinToString(". ").ifEmpty { "Nothing to add" }
    }
}

/** Thrown by [Streams.copyWithin] when the source is longer than the room left. */
class OutOfRoom : IOException("phone storage low")

object Streams {
    /**
     * Copy [input] to [output], at most [maxBytes]. A source that streams on past the room left (a provider that gave
     * no size, or lied) stops at once with [OutOfRoom]: one shared file must never fill the phone.
     * Returns the bytes copied.
     */
    fun copyWithin(input: java.io.InputStream, output: java.io.OutputStream, maxBytes: Long): Long {
        val buf = ByteArray(64 * 1024)
        var total = 0L
        while (true) {
            val n = input.read(buf)
            if (n < 0) return total
            total += n
            if (total > maxBytes) throw OutOfRoom()
            output.write(buf, 0, n)
        }
    }
}

/** Share > Tugboat: streamed copy of each shared URI into app storage, then the core. Any file type. */
object ShareImporter {
    /** Keep this much free after the copy: a full phone breaks more than Tugboat. */
    private const val FREE_MARGIN = 64L * 1024 * 1024

    /** Blocking. One bad file never stops the others; each refusal is named in the result. [intentType] = the share intent's MIME type. */
    fun run(c: Context, uris: List<Uri>, intentType: String? = null): ShareResult {
        val target = Capture.importTarget(c)
        val importDir = File(c.cacheDir, "import").apply { mkdirs() }
        var added = 0
        val refused = mutableListOf<String>()
        for (uri in uris) {
            val label = displayName(c, uri) ?: "a file"
            try {
                if (!ShareNames.acceptableUri(uri.scheme, uri.authority, "${c.packageName}.fileprovider")) throw Refusal("not a shared file")
                // Shortcut and folder-setting files never sync: say so before a single byte is copied.
                if (ShareNames.refused(label)) throw Refusal(ShareNames.REFUSED_SHORT)
                if (importOne(c, uri, label, intentType, importDir, target.folderId)) added++
            } catch (e: Refusal) {
                // The toast is gone in seconds: the refusal also goes to the error list, which stays until dismissed.
                c.core.reportError("share $label: ${if (e.message == ShareNames.REFUSED_SHORT) ShareNames.REFUSED_WHY else e.message}")
                refused += "$label (${e.message})"
            } catch (e: Exception) {
                // Core errors (Refused, NotMedia, Io, Invalid) carry a readable one-line message.
                c.core.reportError("share $label: ${e.message}")
                refused += "$label (${e.message})"
            }
        }
        return ShareResult(added, refused, target.folderName)
    }

    private class Refusal(msg: String) : Exception(msg)

    private fun importOne(c: Context, uri: Uri, label: String, intentType: String?, dir: File, folderId: String): Boolean {
        val resolver = c.contentResolver
        val declared = size(c, uri)
        val free = c.core.phone().freeSpaceBytes().toLong()
        if (declared != null && declared + FREE_MARGIN > free) throw Refusal("phone storage low")
        val tmp = File(dir, "${UUID.randomUUID()}.tmp")
        var media: Media? = null
        try {
            val input = resolver.openInputStream(uri) ?: throw Refusal("cannot be opened")
            input.use { ins ->
                val head = ByteArray(MagicBytes.HEAD_LEN)
                var n = 0
                while (n < head.size) {
                    val r = ins.read(head, n, head.size - n)
                    if (r < 0) break
                    n += r
                }
                // A photo or video is known by its first bytes (and a name that does not say otherwise: an
                // .m4a is audio); anything else is a plain file.
                media = ShareNames.mediaFor(label, MagicBytes.sniff(head, n))
                tmp.outputStream().use { out ->
                    out.write(head, 0, n)
                    Streams.copyWithin(ins, out, free - FREE_MARGIN - n)
                }
            }
        } catch (e: Exception) {
            tmp.delete()
            if (e is Refusal) throw e
            if (e is OutOfRoom) throw Refusal(e.message ?: "phone storage low")
            throw IOException("could not be read: ${e.message}", e)
        }
        val fallback = "Shared_${System.currentTimeMillis()}"
        val mime = ShareQuery.type(resolver, uri) ?: intentType
        val name = ShareNames.finalName(label, media, if (media == null) FileRules.extFromMime(mime, PlatformMime.extOf) else null, fallback)
        try {
            if (ShareNames.refused(name)) throw Refusal(ShareNames.REFUSED_SHORT)
            Capture.add(c, tmp, name, folderId, System.currentTimeMillis(), null, imported = true, video = media?.video == true, thumbnail = media != null)
        } catch (e: Exception) {
            tmp.delete() // the sender still has the original; our temp copy is only clutter
            throw e
        }
        return true
    }

    private fun displayName(c: Context, uri: Uri): String? = query(c, uri, OpenableColumns.DISPLAY_NAME) { it.getString(0) }

    private fun size(c: Context, uri: Uri): Long? = query(c, uri, OpenableColumns.SIZE) { if (it.isNull(0)) null else it.getLong(0) }

    private fun <T> query(c: Context, uri: Uri, col: String, read: (android.database.Cursor) -> T?): T? = try {
        c.contentResolver.query(uri, arrayOf(col), null, null, null)?.use { if (it.moveToFirst()) read(it) else null }
    } catch (e: Exception) {
        null // Some providers refuse the query; the name falls back to Shared_<time>, the size to unknown.
    }
}
