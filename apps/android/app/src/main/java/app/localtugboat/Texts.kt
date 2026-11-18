package app.localtugboat

import uniffi.tugboat_ffi.ItemStateInfo
import uniffi.tugboat_ffi.SnapException
import uniffi.tugboat_ffi.SyncState
import uniffi.tugboat_ffi.SyncStatus

/** All user-facing status wording that is computed from core values. Pure: no Android, no clock. */
object Texts {
    private const val MIN = 60_000L
    private const val HOUR = 60 * MIN
    private const val DAY = 24 * HOUR

    /** "5h", "42m", "3d". Rounds down; under a minute is "<1m". */
    fun duration(ms: Long): String = when {
        ms < MIN -> "<1m"
        ms < HOUR -> "${ms / MIN}m"
        ms < 2 * DAY -> "${ms / HOUR}h"
        else -> "${ms / DAY}d"
    }

    /** "10s ago", "3m ago", "5h ago". Under 5 s is "just now". A future time (clock change) counts as now. */
    fun ago(ms: Long): String = when {
        ms < 5_000 -> "just now"
        ms < MIN -> "${ms / 1000}s ago"
        else -> "${duration(ms)} ago"
    }

    /** Label under a thumbnail in the gallery. */
    fun expiryLabel(state: ItemStateInfo, nowMs: Long, binDays: UInt): String = when (state) {
        is ItemStateInfo.Live -> "Expires in ${duration(state.expiresAtMs - nowMs)}"
        ItemStateInfo.Kept -> "Kept"
        ItemStateInfo.FolderKeep -> "Kept (folder)"
        is ItemStateInfo.Deleted -> binLabel(state.binnedAtMs, nowMs, binDays)
        is ItemStateInfo.Expired -> binLabel(state.binnedAtMs, nowMs, binDays)
    }

    /** Bin: days left before this phone deletes its copy. Rounds up so day 1 of 7 reads "7d left". */
    fun binLabel(binnedAtMs: Long, nowMs: Long, binDays: UInt): String {
        val leftMs = binnedAtMs + binDays.toLong() * DAY - nowMs
        if (leftMs <= 0) return "In Bin, removing soon"
        return "In Bin, ${(leftMs + DAY - 1) / DAY}d left"
    }

    /** Banner after a heartbeat gap: "STOPPED at 14:02 - Tugboat was not running". */
    fun stoppedAt(fromMs: Long, zone: java.time.ZoneId): String {
        val hm = java.time.format.DateTimeFormatter.ofPattern("HH:mm", java.util.Locale.ROOT)
        return "STOPPED at ${java.time.Instant.ofEpochMilli(fromMs).atZone(zone).format(hm)} - Tugboat was not running"
    }

    /** The ongoing notification line: "Connected to NIGEL-PC, synced 10s ago" or "PC not found, 3 waiting". */
    fun notification(s: SyncStatus, nowMs: Long): String {
        val pc = s.peerName ?: "PC"
        return when (s.state) {
            SyncState.UNPAIRED -> "Not paired"
            SyncState.SYNCING -> "Syncing with $pc"
            SyncState.SYNCED -> {
                val at = s.lastSyncedAt
                if (at == null) "Connected to $pc" else "Connected to $pc, synced ${ago(nowMs - at)}"
            }
            SyncState.OFFLINE, SyncState.CONNECTING -> notFound(s)
            SyncState.PAUSED -> s.message ?: "Sync paused"
            SyncState.REFUSED -> s.message ?: "PC refused the connection"
        }
    }

    /** The small sync line on the camera screen: "Synced 10s ago" or "PC not found, 3 waiting". */
    fun cameraSync(s: SyncStatus, nowMs: Long): String = when (s.state) {
        SyncState.UNPAIRED -> "Not paired"
        SyncState.SYNCING -> "Syncing"
        SyncState.SYNCED -> s.lastSyncedAt?.let { "Synced ${ago(nowMs - it)}" } ?: "Connected"
        SyncState.OFFLINE, SyncState.CONNECTING -> notFound(s)
        SyncState.PAUSED -> s.message ?: "Sync paused"
        SyncState.REFUSED -> s.message ?: "PC refused the connection"
    }

    /** One line for a failed pairing. The core's own messages are already readable; only the network ones need context. */
    fun pairFailure(e: Exception, localNetworkAllowed: Boolean, codeExpired: Boolean = false): String = when (e) {
        // The PC opens a NEW pairing window when the old one runs out, so an old QR reaches it as a wrong secret or a closed
        // window. When the code's own expiry time has passed by this phone's clock, that is what it means.
        is SnapException.PairBadSecret, is SnapException.PairNotOpen ->
            if (codeExpired) EXPIRED_QR else e.message ?: "WRONG PAIRING CODE - scan the QR again"
        // The core already says "PAIRING CODE EXPIRED - show a new QR": show its words.
        is SnapException.PairExpired -> e.message ?: EXPIRED_QR
        is SnapException.PairBadPayload -> "That is not a Tugboat code"
        is SnapException.NetTimeout, is SnapException.NetUnreachable ->
            if (localNetworkAllowed) "PC NOT FOUND - same Wi-Fi as the PC?" else "LOCAL NETWORK BLOCKED - allow it, then scan again"
        else -> e.message ?: e.javaClass.simpleName
    }

    const val EXPIRED_QR = "PAIRING CODE EXPIRED - show a new QR"

    /** Red banner while the core holds downloads back because free space is under the storage guard. */
    fun storageLow(waitingOnPc: UInt): String = when (waitingOnPc) {
        0u -> "PHONE STORAGE LOW"
        1u -> "PHONE STORAGE LOW - 1 item waits on the PC"
        else -> "PHONE STORAGE LOW - $waitingOnPc items wait on the PC"
    }

    /** Bin note for an item that expired before the PC ever confirmed holding it. */
    const val NOT_ON_PC = "NOT ON PC"

    private fun notFound(s: SyncStatus): String =
        if (s.waiting > 0u) "PC not found, ${s.waiting} waiting" else "PC not found"
}
