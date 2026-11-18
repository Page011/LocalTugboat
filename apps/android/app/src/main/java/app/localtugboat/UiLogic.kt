package app.localtugboat

import uniffi.tugboat_ffi.ActivityAction
import uniffi.tugboat_ffi.ItemInfo
import uniffi.tugboat_ffi.LocalFileState
import uniffi.tugboat_ffi.MediaKind
import uniffi.tugboat_ffi.SyncState
import uniffi.tugboat_ffi.SyncStatus

/*
 * Pure mapping from core/platform state to what the screens show. No Android, no Compose, no clock:
 * every rule here has a test in UiLogicTest.
 */

// ---- navigation ----

sealed interface Route {
    data object Camera : Route
    data class Gallery(val folderId: String?) : Route
    data object Bin : Route
    data object Activity : Route
    data object Settings : Route
    /** [folderId] null = all folders. [bin] true = list is the Bin. */
    data class Viewer(val folderId: String?, val bin: Boolean, val itemId: String) : Route
}

enum class Root { LOADING, FATAL, ONBOARDING, MAIN }

enum class PermKind { CAMERA, LOCAL_NETWORK, NOTIFICATIONS, LOCATION, MICROPHONE }
enum class PermStatus { GRANTED, NEEDED, BLOCKED }
class PermRow(val kind: PermKind, val title: String, val why: String, val status: PermStatus, val required: Boolean)

enum class OnboardingStep { PERMISSIONS, SCAN, CONNECTING, CODE, FAILED, DONE }

enum class FailAction { ALLOW, RETRY }

object UiLogic {
    const val NOTHING_YET = "Nothing yet. Photos you take here show up on your PC."

    /** Which top-level screen. The camera opens only once a PC is paired: a shot with nowhere to go is a trap. */
    fun root(ui: UiState): Root = when {
        !ui.loaded -> Root.LOADING
        ui.openError != null -> Root.FATAL
        ui.paired == null -> Root.ONBOARDING
        else -> Root.MAIN
    }

    // ---- permissions ----

    /** The runtime permission behind a result entry, or null for one the app does not track. */
    fun permKind(permission: String): PermKind? = when (permission) {
        "android.permission.CAMERA" -> PermKind.CAMERA
        "android.permission.ACCESS_LOCAL_NETWORK" -> PermKind.LOCAL_NETWORK
        "android.permission.POST_NOTIFICATIONS" -> PermKind.NOTIFICATIONS
        "android.permission.RECORD_AUDIO" -> PermKind.MICROPHONE
        "android.permission.ACCESS_FINE_LOCATION", "android.permission.ACCESS_COARSE_LOCATION" -> PermKind.LOCATION
        else -> null
    }

    /**
     * Rows of the permission screen. A permission the system will not ask for again ([hardDenied]) reads
     * BLOCKED: the button then opens app settings. Local network exists only from API 37.
     */
    fun permRows(p: PermState, hardDenied: Set<PermKind>, sdk: Int, withMic: Boolean = false): List<PermRow> {
        fun st(ok: Boolean, k: PermKind) = if (ok) PermStatus.GRANTED else if (k in hardDenied) PermStatus.BLOCKED else PermStatus.NEEDED
        return buildList {
            add(PermRow(PermKind.CAMERA, "Camera", "Take photos and scan the QR code", st(p.camera, PermKind.CAMERA), true))
            if (sdk >= 37) add(PermRow(PermKind.LOCAL_NETWORK, "Local network", "Find your PC on Wi-Fi", st(p.localNetwork, PermKind.LOCAL_NETWORK), true))
            add(PermRow(PermKind.NOTIFICATIONS, "Notifications", "Show sync state while Tugboat runs", st(p.notifications, PermKind.NOTIFICATIONS), false))
            add(PermRow(PermKind.LOCATION, "Location (optional)", "Sort shots into places", st(p.location != LocationAccess.NONE, PermKind.LOCATION), false))
            if (withMic) add(PermRow(PermKind.MICROPHONE, "Microphone (optional)", "Sound in videos", st(p.microphone, PermKind.MICROPHONE), false))
        }
    }

    /** Camera and local network gate pairing. Notifications and location never block. */
    fun requiredMet(p: PermState) = p.camera && p.localNetwork

    /** Permissions are listed until the required two are on AND the user pressed Continue or has nothing left to allow. */
    fun showPermissionScreen(rows: List<PermRow>, continued: Boolean): Boolean =
        rows.any { it.required && it.status != PermStatus.GRANTED } || (!continued && rows.any { it.status == PermStatus.NEEDED })

    fun onboardingStep(rows: List<PermRow>?, continued: Boolean, pairing: PairUi): OnboardingStep = when (pairing) {
        PairUi.Dialing -> OnboardingStep.CONNECTING
        is PairUi.Code -> OnboardingStep.CODE
        is PairUi.Failed -> OnboardingStep.FAILED
        is PairUi.Done -> OnboardingStep.DONE
        PairUi.Idle -> if (rows == null || showPermissionScreen(rows, continued)) OnboardingStep.PERMISSIONS else OnboardingStep.SCAN
    }

    /** The system permission names to ask for one row. Fine and coarse location are asked together (the user may grant coarse only). */
    fun permissionsFor(kind: PermKind): Array<String> = when (kind) {
        PermKind.CAMERA -> arrayOf("android.permission.CAMERA")
        PermKind.LOCAL_NETWORK -> arrayOf("android.permission.ACCESS_LOCAL_NETWORK")
        PermKind.NOTIFICATIONS -> arrayOf("android.permission.POST_NOTIFICATIONS")
        PermKind.LOCATION -> arrayOf("android.permission.ACCESS_FINE_LOCATION", "android.permission.ACCESS_COARSE_LOCATION")
        PermKind.MICROPHONE -> arrayOf("android.permission.RECORD_AUDIO")
    }

    /** The permission kinds that are on right now. */
    fun grantedKinds(p: PermState): Set<PermKind> = buildSet {
        if (p.camera) add(PermKind.CAMERA)
        if (p.localNetwork) add(PermKind.LOCAL_NETWORK)
        if (p.notifications) add(PermKind.NOTIFICATIONS)
        if (p.location != LocationAccess.NONE) add(PermKind.LOCATION)
        if (p.microphone) add(PermKind.MICROPHONE)
    }

    /**
     * Kinds that will never show a dialog again after a request: still not granted AND (the system gave no rationale,
     * which after a denial means "don't ask again", or it is notifications, where a switch in settings can be off
     * with the permission itself granted). Kinds granted now are dropped.
     */
    fun hardDenied(prev: Set<PermKind>, requested: Set<PermKind>, noRationale: Set<PermKind>, now: PermState): Set<PermKind> {
        val granted = grantedKinds(now)
        val added = requested.filter { it !in granted && (it in noRationale || it == PermKind.NOTIFICATIONS) }
        return (prev + added) - granted
    }

    /** Allow on a BLOCKED row opens app settings; on a NEEDED row it asks. */
    fun opensSettings(s: PermStatus) = s == PermStatus.BLOCKED

    /** "482913" -> "482 913" so the code can be compared with the PC at a glance. */
    fun formatCode(code: String): String {
        val s = code.trim()
        return if (s.length == 6 && s.all { it.isDigit() }) "${s.take(3)} ${s.drop(3)}" else s
    }

    /** A failed pairing the user can fix with a permission gets [FailAction.ALLOW]; everything else scans again. */
    fun failAction(message: String) = if (message.startsWith("LOCAL NETWORK BLOCKED")) FailAction.ALLOW else FailAction.RETRY

    // ---- banners ----

    enum class Level { BLOCKER, WARNING }
    enum class BannerId { OPEN_ERROR, LOCAL_NETWORK, STORAGE, STOPPED, NOTIFICATIONS, ERROR, SYNC }

    class BannerUi(val id: BannerId, val text: String, val action: String?, val level: Level)

    /** Banners for a paired phone, most urgent first. Blockers (red) before warnings (amber). */
    fun banners(ui: UiState, zone: java.time.ZoneId): List<BannerUi> {
        val out = mutableListOf<BannerUi>()
        ui.openError?.let { out += BannerUi(BannerId.OPEN_ERROR, it, null, Level.BLOCKER) }
        val perms = ui.perms
        val paired = ui.paired != null
        if (paired && perms != null && !perms.localNetwork) {
            out += BannerUi(BannerId.LOCAL_NETWORK, "LOCAL NETWORK BLOCKED - sync cannot work", "Allow", Level.BLOCKER)
        }
        if (paired && ui.storageLow) out += BannerUi(BannerId.STORAGE, "PHONE STORAGE LOW - downloads from the PC wait", null, Level.BLOCKER)
        ui.stoppedGapFromMs?.let { out += BannerUi(BannerId.STOPPED, Texts.stoppedAt(it, zone), "Fix", Level.WARNING) }
        if (paired && perms != null && !perms.notifications) {
            out += BannerUi(BannerId.NOTIFICATIONS, "Notifications off: sync state shows only in Tugboat", "Allow", Level.WARNING)
        }
        ui.errors.lastOrNull()?.let { out += BannerUi(BannerId.ERROR, it, "Dismiss", Level.WARNING) }
        return out.sortedBy { it.level.ordinal } // stable: keeps the order above inside a level
    }

    /**
     * Banner for a PC that cannot be reached or refuses: lists must not look merely empty then. Red for
     * OFFLINE/CONNECTING/PAUSED/REFUSED (the same "PC not found" the sync text says); none when synced, syncing or unpaired.
     */
    fun syncBanner(s: SyncStatus?): BannerUi? = when (s?.state) {
        SyncState.OFFLINE, SyncState.CONNECTING -> BannerUi(BannerId.SYNC, if (s.waiting > 0u) "PC NOT FOUND - ${s.waiting} waiting" else "PC NOT FOUND", null, Level.BLOCKER)
        SyncState.PAUSED -> BannerUi(BannerId.SYNC, s.message ?: "SYNC PAUSED", null, Level.BLOCKER)
        SyncState.REFUSED -> BannerUi(BannerId.SYNC, s.message ?: "PC REFUSED THE CONNECTION", null, Level.BLOCKER)
        else -> null
    }

    // ---- camera ----

    enum class Flash { OFF, AUTO, ON }

    /** Photo cycles off, auto, on. Video has a torch only: off and on. */
    fun nextFlash(f: Flash, video: Boolean): Flash = when {
        video -> if (f == Flash.ON) Flash.OFF else Flash.ON
        f == Flash.OFF -> Flash.AUTO
        f == Flash.AUTO -> Flash.ON
        else -> Flash.OFF
    }

    /** Lens chips for the zoom range: the ultra-wide end if below 1x, 1x, then 2x and 5x if reachable. */
    fun lensChips(minZoom: Float, maxZoom: Float): List<Float> = buildList {
        if (minZoom < 0.95f) add(minZoom)
        add(1f)
        if (maxZoom >= 2f) add(2f)
        if (maxZoom >= 5f) add(5f)
    }

    fun zoomLabel(ratio: Float): String {
        val r = Math.round(ratio * 10) / 10f
        return if (r == r.toInt().toFloat()) "${r.toInt()}x" else "${r}x"
    }

    /** Recording clock: "0:07", "12:30", "1:02:03". */
    fun clock(ms: Long): String {
        val s = (ms / 1000).coerceAtLeast(0)
        val h = s / 3600
        val m = (s % 3600) / 60
        val sec = s % 60
        return if (h > 0) "%d:%02d:%02d".format(h, m, sec) else "%d:%02d".format(m, sec)
    }

    /** The core's own rule (under 2 minutes old, 100 m or better) applied again before a location is stored on a shot. */
    fun usableFix(accuracyM: Double, ageMs: Long) = ageMs in 0 until 120_000 && accuracyM > 0 && accuracyM <= 100.0

    /**
     * Folder id for a shot at the moment of the shutter, or null = decide at hand-off (place rule). The chip's
     * [targetFolderId] is recomputed off the main thread after a pick, so it only counts when it was computed for
     * the pick that is live now ([targetPick]) and the folder is still there (it may have just been deleted).
     * A live pick that still exists always wins.
     */
    fun shotFolder(pick: String?, folderIds: Collection<String>, targetFolderId: String?, targetPick: String?): String? = when {
        pick != null && pick in folderIds -> pick
        targetFolderId != null && targetPick == pick && targetFolderId in folderIds -> targetFolderId
        else -> null
    }

    /**
     * What a decoded full-size photo depends on. A held file keeps its path when its content changes (a new
     * version replaces it in place), so size and the hash-named thumbnail path stand in for a content revision.
     * Moves, pins and state changes leave all three alone, so they never re-decode.
     */
    fun photoDecodeKey(path: String, size: ULong, thumbPath: String?): List<Any?> = listOf(path, size, thumbPath)

    /** Saved-state keys of viewer screens that were seen but are no longer anywhere on the stack: their state is dropped. */
    fun droppedViewerKeys(seen: Set<Route>, current: List<Route>): Set<String> =
        seen.filterIsInstance<Route.Viewer>().filter { it !in current }.map { it.toString() }.toSet()

    class ChipUi(val text: String, val noLocation: Boolean)

    /**
     * Viewfinder folder chip. No target yet = loading, shown as Inbox. "No location" when places exist and
     * there is no usable fix (the shot goes to Unsorted); a folder picked by hand shows that folder.
     */
    fun chip(target: Target?): ChipUi = when {
        target == null -> ChipUi("Inbox", false)
        target.noLocation -> ChipUi("No location", true)
        else -> ChipUi(target.folderName, false)
    }

    enum class Tone { OK, BUSY, WARN, BAD }

    /** Dot colour next to the sync text. The text always says the same thing, colour is only a hint. */
    fun tone(s: SyncStatus?): Tone = when (s?.state) {
        SyncState.SYNCED -> Tone.OK
        SyncState.SYNCING -> Tone.BUSY
        // CONNECTING reads "PC not found" in the sync text too (Texts.cameraSync), so its dot is red as well.
        SyncState.OFFLINE, SyncState.CONNECTING, SyncState.PAUSED, SyncState.REFUSED -> Tone.BAD
        else -> Tone.WARN
    }

    // ---- gallery ----

    sealed interface ListUi {
        data object Loading : ListUi
        data class Failed(val message: String) : ListUi
        data class Empty(val text: String) : ListUi
        data object Content : ListUi
    }

    /** What a list screen shows. A load error with rows already on screen keeps the rows. */
    fun listUi(count: Int, loaded: Boolean, error: String?, folderName: String?, bin: Boolean): ListUi = when {
        count > 0 -> ListUi.Content
        error != null -> ListUi.Failed(error)
        !loaded -> ListUi.Loading
        bin -> ListUi.Empty("Bin is empty.")
        folderName != null -> ListUi.Empty("Nothing in $folderName.")
        else -> ListUi.Empty(NOTHING_YET)
    }

    /** What a folder does to its items: "Kept" or "Expires after 7d". */
    fun retention(secs: ULong?): String = if (secs == null) "Kept" else "Expires after ${Texts.duration(secs.toLong() * 1000)}"

    /** Why a file is not viewable here, or null when it is held. */
    fun fileNote(local: LocalFileState, bin: Boolean): String? = when (local) {
        LocalFileState.PENDING -> "Downloading from PC"
        LocalFileState.LOST -> "FILE MISSING on this phone"
        LocalFileState.REFUSED -> "PC refused this file"
        LocalFileState.PURGED -> if (bin) null else "On PC only"
        LocalFileState.PRESENT, LocalFileState.BINNED -> null
    }

    /** Share and Save need the file on this phone. */
    fun fileHeld(item: ItemInfo) = item.path != null && item.local == LocalFileState.PRESENT

    /** Furthest the zoomed photo may be dragged from centre on one axis: its edge stops at the screen edge. */
    fun panLimit(scale: Float, extent: Float) = if (scale <= 1f) 0f else extent * (scale - 1f) / 2f

    class CellUi(val label: String, val kept: Boolean, val video: Boolean, val waiting: Boolean)

    /** Label and badges under one thumbnail. */
    fun cell(item: ItemInfo, nowMs: Long, binDays: UInt, bin: Boolean): CellUi {
        val label = fileNote(item.local, bin) ?: Texts.expiryLabel(item.state, nowMs, binDays)
        val kept = item.pinned || item.state is uniffi.tugboat_ffi.ItemStateInfo.Kept || item.state is uniffi.tugboat_ffi.ItemStateInfo.FolderKeep
        return CellUi(label, kept, item.kind == MediaKind.VIDEO, item.state is uniffi.tugboat_ffi.ItemStateInfo.Waiting)
    }

    /** "17 Nov, 10:30" in the phone's own zone and language. */
    fun capturedAt(ms: Long, zone: java.time.ZoneId, locale: java.util.Locale = java.util.Locale.getDefault()): String =
        java.time.format.DateTimeFormatter.ofPattern("d MMM, HH:mm", locale).format(java.time.Instant.ofEpochMilli(ms).atZone(zone))

    /** Activity row time: relative within a day ("5m ago"), else the date. */
    fun activityTime(atMs: Long, nowMs: Long, zone: java.time.ZoneId, locale: java.util.Locale = java.util.Locale.getDefault()): String =
        if (nowMs - atMs < 24 * 60 * 60 * 1000L) Texts.ago(nowMs - atMs) else capturedAt(atMs, zone, locale)

    // ---- activity ----

    fun activityText(a: ActivityAction): String = when (a) {
        ActivityAction.CAPTURED -> "Captured"
        ActivityAction.ADDED -> "Added from phone"
        ActivityAction.RECEIVED -> "Received from PC"
        ActivityAction.MOVED -> "Moved"
        ActivityAction.RENAMED -> "Renamed"
        ActivityAction.KEPT -> "Kept"
        ActivityAction.UNKEPT -> "No longer kept"
        ActivityAction.EDITED -> "Edited"
        ActivityAction.DELETED -> "Deleted"
        ActivityAction.BINNED -> "Moved to Bin"
        ActivityAction.EXPIRED -> "Expired"
        ActivityAction.RESTORED -> "Restored"
        ActivityAction.RETURNED -> "Back from Bin"
        ActivityAction.PURGED -> "Removed from Bin"
        ActivityAction.REFUSED -> "REFUSED"
        ActivityAction.CONFLICT -> "CONFLICT"
        ActivityAction.LOST -> "FILE MISSING"
        ActivityAction.FOLDER_CREATED -> "Folder created"
        ActivityAction.FOLDER_RENAMED -> "Folder renamed"
        ActivityAction.FOLDER_DELETED -> "Folder deleted"
        ActivityAction.RETENTION_CHANGED -> "Expiry changed"
        ActivityAction.PAIRED -> "Paired"
        ActivityAction.UNPAIRED -> "Unpaired"
        ActivityAction.PAUSED -> "SYNC PAUSED"
        ActivityAction.RESUMED -> "Sync resumed"
        ActivityAction.SYNC_ERROR -> "SYNC ERROR"
        ActivityAction.UPGRADED -> "Upgraded"
    }

    /** Actions that need the user's attention are shown in the error colour with the word in capitals. */
    fun activityIsProblem(a: ActivityAction) = when (a) {
        ActivityAction.REFUSED, ActivityAction.CONFLICT, ActivityAction.LOST, ActivityAction.SYNC_ERROR, ActivityAction.PAUSED -> true
        else -> false
    }

    // ---- settings ----

    /** "12.3 GB", "850 MB": storage line in Settings. */
    fun bytes(n: Long): String = when {
        n >= 1L shl 30 -> "%.1f GB".format(java.util.Locale.ROOT, n / (1L shl 30).toDouble())
        n >= 1L shl 20 -> "${n shr 20} MB"
        else -> "${n shr 10} KB"
    }

    /** Bin day choices; a value set elsewhere (the PC) stays selectable so the screen never hides the real setting. */
    fun binChoices(current: UInt): List<UInt> = (listOf(1u, 3u, 7u, 14u, 30u) + current).distinct().sorted()
}
