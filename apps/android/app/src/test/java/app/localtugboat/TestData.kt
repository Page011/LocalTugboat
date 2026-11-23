package app.localtugboat

import uniffi.tugboat_ffi.DeviceStatus
import uniffi.tugboat_ffi.SyncState
import uniffi.tugboat_ffi.SyncStatus

/** Status values the UI tests share. */
object TestData {
    /** 64 hex characters derived from the name, so two names never share an id. */
    private fun idOf(name: String) = name.map { "%02x".format(it.code) }.joinToString("").padEnd(64, '0').take(64)

    fun device(
        name: String = "OFFICE-PC", state: SyncState = SyncState.SYNCED, message: String? = null, lastSeenAt: Long? = null,
        lastSyncedAt: Long? = null, waiting: UInt = 0u, downloading: UInt = 0u, unlinking: Boolean = false,
    ) = DeviceStatus(idOf(name), name, state, message, 1L, lastSeenAt, lastSyncedAt, waiting, downloading, unlinking)

    /** The aggregate the core reports. With no [devices] given, one device in the same state (none when unpaired). */
    fun status(
        state: SyncState, waiting: UInt = 0u, message: String? = null, downloading: UInt = 0u, storageLow: Boolean = false,
        lastSyncedAt: Long? = null, devices: List<DeviceStatus>? = null,
    ): SyncStatus {
        val list = devices ?: if (state == SyncState.UNPAIRED) emptyList() else listOf(device(state = state, message = message, lastSyncedAt = lastSyncedAt))
        val peer = list.singleOrNull { !it.unlinking }?.name
        return SyncStatus(state, peer, lastSyncedAt, waiting, downloading, storageLow, message, list)
    }
}
