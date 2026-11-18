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
        ItemStateInfo.Waiting -> "Waiting for PC"
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

    /** Banner after a heartbeat gap: "Tugboat was stopped at 14:02." */
    fun stoppedAt(fromMs: Long, zone: java.time.ZoneId): String {
        val hm = java.time.format.DateTimeFormatter.ofPattern("HH:mm", java.util.Locale.ROOT)
        return "Tugboat was stopped at ${java.time.Instant.ofEpochMilli(fromMs).atZone(zone).format(hm)}."
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
    fun pairFailure(e: Exception, localNetworkAllowed: Boolean): String = when (e) {
        is SnapException.PairBadPayload -> "That is not a Tugboat code"
        is SnapException.NetTimeout, is SnapException.NetUnreachable ->
            if (localNetworkAllowed) "PC NOT FOUND - same Wi-Fi as the PC?" else "LOCAL NETWORK BLOCKED - allow it, then scan again"
        else -> e.message ?: e.javaClass.simpleName
    }

    private fun notFound(s: SyncStatus): String =
        if (s.waiting > 0u) "PC not found, ${s.waiting} waiting" else "PC not found"
}
