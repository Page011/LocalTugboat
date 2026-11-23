package app.localtugboat

import uniffi.tugboat_ffi.ActivityAction
import uniffi.tugboat_ffi.DeviceStatus
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
    /** Scan another PC's QR code from inside the app. */
    data object AddDevice : Route
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
    const val NOTHING_YET_SEVERAL = "Nothing yet. Photos you take here go to your linked PCs."

    fun nothingYet(several: Boolean) = if (several) NOTHING_YET_SEVERAL else NOTHING_YET

    /** Which top-level screen. The camera opens only once a PC is linked: a shot with nowhere to go is a trap. */
    fun root(ui: UiState): Root = when {
        !ui.loaded -> Root.LOADING
        ui.openError != null -> Root.FATAL
        !linked(ui.devices) -> Root.ONBOARDING
        else -> Root.MAIN
    }

    // ---- linked devices ----

    /** At least one PC is linked (a row that is only waiting to be told it was unlinked does not count). */
    fun linked(devices: List<DeviceStatus>) = Texts.linkedCount(devices) > 0

    /** Settings section title: "Linked devices  3 of 5". */
    fun linkedHeader(devices: List<DeviceStatus>) = "Linked devices  ${Texts.linkedCount(devices)} of ${Texts.MAX_LINKED}"

    class LinkButtonUi(val enabled: Boolean, val limitLine: String?)

    /** "Link a PC" is dimmed at the limit, with the reason under it. */
    fun linkButton(devices: List<DeviceStatus>): LinkButtonUi =
        if (atLimit(devices)) LinkButtonUi(false, Texts.LIMIT_LINE) else LinkButtonUi(true, null)

    fun atLimit(devices: List<DeviceStatus>) = Texts.linkedCount(devices) >= Texts.MAX_LINKED

    /** [away] is grey: being away is normal. [problem] is red. A row that is being unlinked has no button. */
    class DeviceLineUi(val text: String, val problem: Boolean, val away: Boolean, val waitingChip: String?, val canUnlink: Boolean)

    fun deviceLine(d: DeviceStatus, nowMs: Long): DeviceLineUi {
        if (d.unlinking) return DeviceLineUi("Unlinking - tells it when reachable", problem = false, away = false, waitingChip = null, canUnlink = false)
        val chip = if (d.waiting > 0u) "${d.waiting} waiting" else null
        fun line(text: String, problem: Boolean = false, away: Boolean = false) = DeviceLineUi(text, problem, away, chip, canUnlink = true)
        return when (d.state) {
            SyncState.REFUSED -> line(d.message ?: "PC REFUSED THE CONNECTION", problem = true)
            SyncState.SYNCING -> line(if (d.downloading > 0u) "Syncing - ${d.downloading} downloading" else "Syncing")
            SyncState.SYNCED -> line(d.lastSyncedAt?.let { "Synced ${Texts.ago(nowMs - it)}" } ?: "Connected")
            SyncState.PAUSED -> line(d.message ?: "Sync paused", problem = true)
            SyncState.OFFLINE, SyncState.CONNECTING, SyncState.UNPAIRED ->
                line("Not found - " + (d.lastSeenAt?.let { "last seen ${Texts.ago(nowMs - it)}" } ?: "never seen"), away = true)
        }
    }

    /** The core is looking for a linked PC it cannot reach: the radio stays on in bursts for mDNS. Not while paused. */
    fun searching(s: SyncStatus?): Boolean =
        s != null && s.state != SyncState.PAUSED && s.devices.any { !it.unlinking && (it.state == SyncState.OFFLINE || it.state == SyncState.CONNECTING) }

    /** A catch-up run is done: some PC synced since [startedAtMs] and no connected PC still has files for us. */
    fun caughtUp(s: SyncStatus?, startedAtMs: Long): Boolean =
        s != null && s.state == SyncState.SYNCED && (s.lastSyncedAt ?: 0L) >= startedAtMs - 1000 &&
            s.devices.none { (it.state == SyncState.SYNCED || it.state == SyncState.SYNCING) && it.downloading > 0u }

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
            add(PermRow(PermKind.NOTIFICATIONS, "Notifications", "Alert when sync needs you", st(p.notifications, PermKind.NOTIFICATIONS), false))
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

    /**
     * Pairing state once the pairing is gone. The "Linked to X" screen is a success that is over; left in place it
     * would be the first screen shown. A pairing in progress (Dialing, Code) is a new one and stays.
     */
    fun pairingAfterUnpair(p: PairUi): PairUi = if (p is PairUi.Done) PairUi.Idle else p

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
        val paired = linked(ui.devices)
        if (paired && perms != null && !perms.localNetwork) {
            out += BannerUi(BannerId.LOCAL_NETWORK, "LOCAL NETWORK BLOCKED - sync cannot work", "Allow", Level.BLOCKER)
        }
        // The core enforces the guard and says when it holds downloads back: the banner follows the core, not the free-bytes guess.
        val st = ui.status
        if (paired && st != null && st.storageLow) out += BannerUi(BannerId.STORAGE, Texts.storageLow(st.downloading), null, Level.BLOCKER)
        ui.stoppedGapFromMs?.let { out += BannerUi(BannerId.STOPPED, Texts.stoppedAt(it, zone), "Fix", Level.WARNING) }
        if (paired && perms != null && !perms.notifications) {
            out += BannerUi(BannerId.NOTIFICATIONS, "Notifications off: sync problems show only in Tugboat", "Allow", Level.WARNING)
        }
        ui.errors.lastOrNull()?.let { out += BannerUi(BannerId.ERROR, it, "Dismiss", Level.WARNING) }
        return out.sortedBy { it.level.ordinal } // stable: keeps the order above inside a level
    }

    /**
     * Banner for a PC that cannot be reached or refuses: lists must not look merely empty then. Red for
     * OFFLINE/CONNECTING/PAUSED/REFUSED (the same "PC not found" the sync text says); none when synced, syncing or unpaired.
     * With several PCs the aggregate is OFFLINE only when none is reachable, so one PC being away never shows it.
     */
    fun syncBanner(s: SyncStatus?): BannerUi? = when (s?.state) {
        SyncState.OFFLINE, SyncState.CONNECTING -> {
            val what = if (Texts.several(s)) "NO PC FOUND" else "PC NOT FOUND"
            BannerUi(BannerId.SYNC, if (s.waiting > 0u) "$what - ${s.waiting} waiting" else what, null, Level.BLOCKER)
        }
        SyncState.PAUSED -> BannerUi(BannerId.SYNC, s.message ?: "SYNC PAUSED", null, Level.BLOCKER)
        SyncState.REFUSED -> BannerUi(BannerId.SYNC, s.message ?: "PC REFUSED THE CONNECTION", null, Level.BLOCKER)
        else -> null
    }

    /**
     * The line the phone alerts with, or null. Only problems the user must fix: being away from the PC's Wi-Fi is
     * normal and never alerts, for one PC or several. A refusal alerts with the core's text, which names the PC
     * ("UPDATE TUGBOAT on LAPTOP"). No counts in the text, so a changing number does not restart the alert.
     */
    fun alert(s: SyncStatus): String? = when {
        s.storageLow -> "PHONE STORAGE LOW - downloads stopped"
        s.state == SyncState.REFUSED -> s.message ?: "PC REFUSED THE CONNECTION"
        s.state == SyncState.PAUSED -> s.message ?: "SYNC PAUSED"
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

    /**
     * Screen orientation request while the camera screen runs: locked at the current orientation during a recording
     * (a rotation would recreate the activity and end the video), free otherwise.
     */
    fun recordingOrientation(recording: Boolean): Int =
        if (recording) android.content.pm.ActivityInfo.SCREEN_ORIENTATION_LOCKED else android.content.pm.ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED

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
    fun listUi(count: Int, loaded: Boolean, error: String?, folderName: String?, bin: Boolean, several: Boolean = false): ListUi = when {
        count > 0 -> ListUi.Content
        error != null -> ListUi.Failed(error)
        !loaded -> ListUi.Loading
        bin -> ListUi.Empty("Bin is empty.")
        folderName != null -> ListUi.Empty("Nothing in $folderName.")
        else -> ListUi.Empty(nothingYet(several))
    }

    /** What a folder does to its items: "Kept" or "Expires after 7d". */
    fun retention(secs: ULong?): String = if (secs == null) "Kept" else "Expires after ${Texts.duration(secs.toLong() * 1000)}"

    const val WAITING_STORAGE_LOW = "Waiting - phone storage low"

    /**
     * Why a file is not viewable here, or null when it is held. While the storage guard holds downloads
     * ([storageLow]) a pending file is not downloading, so it says so.
     */
    fun fileNote(local: LocalFileState, bin: Boolean, storageLow: Boolean = false, several: Boolean = false): String? = when (local) {
        LocalFileState.PENDING -> if (storageLow) WAITING_STORAGE_LOW else if (several) "Downloading" else "Downloading from PC"
        LocalFileState.LOST -> "FILE MISSING on this phone"
        LocalFileState.REFUSED -> if (several) "REFUSED" else "PC refused this file"
        LocalFileState.PURGED -> if (bin) null else if (several) "Not on this phone" else "On PC only"
        LocalFileState.PRESENT, LocalFileState.BINNED -> null
    }

    /** Share and Save need the file on this phone. */
    fun fileHeld(item: ItemInfo) = item.path != null && item.local == LocalFileState.PRESENT

    /** Furthest the zoomed photo may be dragged from centre on one axis: its edge stops at the screen edge. */
    fun panLimit(scale: Float, extent: Float) = if (scale <= 1f) 0f else extent * (scale - 1f) / 2f

    class CellUi(val label: String, val kept: Boolean, val video: Boolean, val file: Boolean)

    /** Label and badges under one thumbnail or file tile. A Bin item that never reached a PC says which. [linked] = PCs linked now. */
    fun cell(item: ItemInfo, nowMs: Long, binDays: UInt, bin: Boolean, storageLow: Boolean = false, linked: Int = 1): CellUi {
        val base = fileNote(item.local, bin, storageLow, linked > 1) ?: Texts.expiryLabel(item.state, nowMs, binDays)
        val label = if (bin && item.neverReachedPeer) "$base - ${Texts.notOn(item.missingOn, linked)}" else base
        val kept = item.pinned || item.state is uniffi.tugboat_ffi.ItemStateInfo.Kept || item.state is uniffi.tugboat_ffi.ItemStateInfo.FolderKeep
        return CellUi(label, kept, item.kind == MediaKind.VIDEO, item.kind == MediaKind.FILE)
    }

    /** Spoken name of a tile or page: "Photo", "Video" or "File". */
    fun kindWord(k: MediaKind): String = when (k) {
        MediaKind.IMAGE -> "Photo"
        MediaKind.VIDEO -> "Video"
        MediaKind.FILE -> "File"
    }

    /** Share and Open need the file on this phone AND outside the Bin (the provider serves items/ only). */
    fun canOpen(item: ItemInfo, bin: Boolean) = !bin && fileHeld(item)

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
        ActivityAction.DELETED_FOREVER -> "Deleted forever"
        ActivityAction.REFUSED -> "REFUSED"
        ActivityAction.CONFLICT -> "CONFLICT"
        ActivityAction.LOST -> "FILE MISSING"
        ActivityAction.FOLDER_CREATED -> "Folder created"
        ActivityAction.FOLDER_RENAMED -> "Folder renamed"
        ActivityAction.FOLDER_DELETED -> "Folder deleted"
        ActivityAction.RETENTION_CHANGED -> "Expiry changed"
        ActivityAction.PAIRED -> "Linked"
        ActivityAction.UNPAIRED -> "Unlinked"
        ActivityAction.PAUSED -> "SYNC PAUSED"
        ActivityAction.RESUMED -> "Sync resumed"
        ActivityAction.SYNC_ERROR -> "SYNC ERROR"
        ActivityAction.UPGRADED -> "Upgraded"
    }

    /** The core words an unlink by the PC as "by the other device"; the phone says what the PC did. */
    fun activityDetail(a: ActivityAction, detail: String): String =
        if (a == ActivityAction.UNPAIRED && detail == "by the other device") "logged out" else detail

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
