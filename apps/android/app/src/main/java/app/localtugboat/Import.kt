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
        val (rotation, flip) = orientation(ExifInterface(src.path).getAttributeInt(ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL))
        return fit(raw, rotation, flip, maxSide)
    }

    /**
     * EXIF orientation value -> (clockwise degrees, mirrored). The mirror is applied first, then the rotation
     * (what ExifInterface.rotationDegrees / isFlipped describe). Mirrored values come from front cameras and
     * from other apps' files. Unknown values mean upright.
     */
    fun orientation(exif: Int): Pair<Float, Boolean> = when (exif) {
        ExifInterface.ORIENTATION_FLIP_HORIZONTAL -> 0f to true
        ExifInterface.ORIENTATION_ROTATE_180 -> 180f to false
        ExifInterface.ORIENTATION_FLIP_VERTICAL -> 180f to true
        ExifInterface.ORIENTATION_TRANSPOSE -> 270f to true
        ExifInterface.ORIENTATION_ROTATE_90 -> 90f to false
        ExifInterface.ORIENTATION_TRANSVERSE -> 90f to true
        ExifInterface.ORIENTATION_ROTATE_270 -> 270f to false
        else -> 0f to false
    }

    private fun videoFrame(src: File): Bitmap? {
        val r = MediaMetadataRetriever()
        try {
            r.setDataSource(src.path)
            val frame = r.getFrameAtTime(0, MediaMetadataRetriever.OPTION_CLOSEST_SYNC) ?: return null
            val rotation = r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_VIDEO_ROTATION)?.toFloatOrNull() ?: 0f
            return fit(frame, rotation, false)
        } finally {
            r.release()
        }
    }

    private fun fit(raw: Bitmap, rotation: Float, flip: Boolean, maxSide: Int = MAX_SIDE): Bitmap {
        val scale = minOf(1f, maxSide.toFloat() / maxOf(raw.width, raw.height))
        if (scale == 1f && rotation == 0f && !flip) return raw
        val m = Matrix().apply { postScale(if (flip) -scale else scale, scale); postRotate(rotation) }
        val out = Bitmap.createBitmap(raw, 0, 0, raw.width, raw.height, m, true)
        if (out !== raw) raw.recycle()
        return out
    }
}

/**
 * What a camera shot needs to be added later exactly as meant: the folder it was aimed at, the moment it was
 * taken and the place. Kept in shots/meta/<shot name>.properties next to the shot until the core takes it, so
 * [Capture.recover] does not drop a shot meant for Keep or a place folder into Inbox with the file's time.
 * Subfolder on purpose: [Capture.recover] only looks at plain files in shots/.
 */
class ShotMeta(val folderId: String?, val atMs: Long, val geo: GeoFix?) {
    fun encode(): String {
        val p = java.util.Properties()
        folderId?.let { p.setProperty("folderId", it) }
        p.setProperty("atMs", atMs.toString())
        geo?.let {
            p.setProperty("lat", it.lat.toString())
            p.setProperty("lon", it.lon.toString())
            p.setProperty("accuracyM", it.accuracyM.toString())
        }
        return java.io.StringWriter().also { p.store(it, null) }.toString()
    }

    companion object {
        /** Null for missing, empty or corrupt text: the caller falls back to the old behaviour, never crashes. */
        fun decode(text: String?): ShotMeta? {
            if (text == null) return null
            return try {
                val p = java.util.Properties().apply { load(java.io.StringReader(text)) }
                val at = p.getProperty("atMs")?.toLongOrNull() ?: return null
                val lat = p.getProperty("lat")?.toDoubleOrNull()
                val lon = p.getProperty("lon")?.toDoubleOrNull()
                val acc = p.getProperty("accuracyM")?.toDoubleOrNull()
                val geo = if (lat != null && lon != null && acc != null && lat.isFinite() && lon.isFinite() && acc.isFinite()) GeoFix(lat, lon, acc) else null
                ShotMeta(p.getProperty("folderId"), at, geo)
            } catch (e: Exception) {
                null
            }
        }

        fun file(shotsDir: File, shot: File) = File(File(shotsDir, "meta"), shot.name + ".properties")

        /** Blocking. A failure is logged and swallowed: the shot itself must still be handed over. */
        fun write(c: Context, shot: File, meta: ShotMeta) {
            try {
                val f = file(Capture.shotsDir(c), shot)
                f.parentFile?.mkdirs()
                f.writeText(meta.encode())
            } catch (e: Exception) {
                KLog.log(c, "shot meta not written: $e")
            }
        }

        fun read(c: Context, shot: File): ShotMeta? = try {
            file(Capture.shotsDir(c), shot).takeIf { it.isFile }?.readText().let { decode(it) }
        } catch (e: Exception) {
            null
        }

        fun delete(c: Context, shot: File) {
            file(Capture.shotsDir(c), shot).delete()
        }
    }
}

/** Where the next shot goes, for the viewfinder chip and for the shutter. */
data class Target(val folderId: String, val folderName: String, val noLocation: Boolean)

object Capture {
    /**
     * Folder for the next shot or import. Order: the folder picked on the chip, else the place rule
     * (smallest radius wins, done by the core), else Unsorted when places exist but there is no usable
     * fix, else Inbox. Blocking (SQLite). Throws when the core cannot open or has no Inbox.
     * [pick] is the chip's pick the answer is for (default: the live one); pass null to ignore the pick.
     */
    fun target(c: Context, fix: LocationFix?, pick: String? = c.core.nextShotFolder.value): Target {
        val p = c.core.phone()
        val folders = p.folders()
        pick?.let { id ->
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
    fun add(c: Context, src: File, name: String, folderId: String?, capturedAtMs: Long, geo: GeoFix?, imported: Boolean, video: Boolean): String {
        // One hand-off per file: [recover] must not take a file a slow hand-off is still working on.
        if (!inFlight.add(src.path)) throw IOException("${src.name} is already being added")
        try {
            return addNow(c, src, name, folderId, capturedAtMs, geo, imported, video)
        } finally {
            inFlight.remove(src.path)
        }
    }

    private val inFlight: MutableSet<String> = java.util.concurrent.ConcurrentHashMap.newKeySet()

    private fun addNow(c: Context, src: File, name: String, folderId: String?, capturedAtMs: Long, geo: GeoFix?, imported: Boolean, video: Boolean): String {
        val thumbDir = File(c.cacheDir, "thumbs").apply { mkdirs() }
        val thumb = Thumbs.make(c, src, video, File(thumbDir, "${UUID.randomUUID()}.jpg"))
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
            ShotMeta.delete(c, src)
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
        val knownFolders by lazy { c.core.phone().folders().map { it.id }.toSet() }
        for (f in shotsDir(c).listFiles().orEmpty()) {
            if (!f.isFile || nowMs - f.lastModified() < minAgeMs || f.path in inFlight) continue
            val video = f.extension.equals("mp4", ignoreCase = true)
            try {
                if (f.length() == 0L) {
                    f.delete() // an empty file is no shot
                    ShotMeta.delete(c, f)
                    continue
                }
                // What the shot was meant for, written at the shutter. No folder (or one that is gone since): the place rule
                // on the stored fix, else Unsorted / Inbox, the same answer the shutter would have given.
                val meta = ShotMeta.read(c, f)
                val folder = meta?.folderId?.takeIf { it in knownFolders }
                    ?: runCatching {
                        target(c, meta?.geo?.let { LocationFix(it.lat, it.lon, it.accuracyM, 0L) }, null).folderId
                    }.getOrNull()
                add(c, f, f.name, folder, meta?.atMs ?: f.lastModified(), meta?.geo, imported = false, video = video)
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
    /** One line: "2 added to Inbox" or "1 added to Inbox. Not synced: x.zip (not a photo or video)". */
    fun summary(): String {
        val ok = if (added > 0) "$added added to $folderName" else ""
        val bad = if (refused.isNotEmpty()) "Not synced: ${refused.joinToString("; ")}" else ""
        return listOf(ok, bad).filter { it.isNotEmpty() }.joinToString(". ").ifEmpty { "Nothing to add" }
    }
}

/** Share > Tugboat: streamed copy of each shared URI into app storage, type by magic bytes, then the core. */
object ShareImporter {
    /** Keep this much free after the copy: a full phone breaks more than Tugboat. */
    private const val FREE_MARGIN = 64L * 1024 * 1024

    /** Blocking. One bad file never stops the others; each refusal is named in the result. */
    fun run(c: Context, uris: List<Uri>): ShareResult {
        val target = Capture.importTarget(c)
        val importDir = File(c.cacheDir, "import").apply { mkdirs() }
        var added = 0
        val refused = mutableListOf<String>()
        for (uri in uris) {
            val label = displayName(c, uri) ?: "a file"
            try {
                if (!ShareNames.acceptableUri(uri.scheme, uri.authority, "${c.packageName}.fileprovider")) throw Refusal("not a shared file")
                if (importOne(c, uri, label, importDir, target.folderId)) added++
            } catch (e: Refusal) {
                refused += "$label (${e.message})"
            } catch (e: Exception) {
                // Core errors (NotMedia, Io, Invalid) carry a readable one-line message.
                c.core.reportError("share $label: ${e.message}")
                refused += "$label (${e.message})"
            }
        }
        return ShareResult(added, refused, target.folderName)
    }

    private class Refusal(msg: String) : Exception(msg)

    private fun importOne(c: Context, uri: Uri, label: String, dir: File, folderId: String): Boolean {
        val resolver = c.contentResolver
        val declared = size(c, uri)
        val free = c.core.phone().freeSpaceBytes().toLong()
        if (declared != null && declared + FREE_MARGIN > free) throw Refusal("phone storage low")
        val tmp = File(dir, "${UUID.randomUUID()}.tmp")
        val media: Media
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
                media = MagicBytes.sniff(head, n) ?: throw Refusal("not a photo or video")
                tmp.outputStream().use { out ->
                    out.write(head, 0, n)
                    ins.copyTo(out)
                }
            }
        } catch (e: Exception) {
            tmp.delete()
            if (e is Refusal) throw e
            throw IOException("could not be read: ${e.message}", e)
        }
        val fallback = "Shared_${System.currentTimeMillis()}"
        val name = ShareNames.finalName(label, media, fallback)
        try {
            Capture.add(c, tmp, name, folderId, System.currentTimeMillis(), null, imported = true, video = media.video)
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
