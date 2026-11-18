package app.localtugboat

import android.app.Application
import android.content.Context
import android.net.wifi.WifiManager
import android.os.Build
import java.io.File
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch
import uniffi.tugboat_ffi.EventListener
import uniffi.tugboat_ffi.FfiEvent
import uniffi.tugboat_ffi.PhoneConfig
import uniffi.tugboat_ffi.PhoneCore
import uniffi.tugboat_ffi.SnapException
import uniffi.tugboat_ffi.SyncState
import uniffi.tugboat_ffi.SyncStatus

/** The process-wide core. One per process: the database and the log file are per data dir. */
val Context.core: Core get() = (applicationContext as SnapApp).core

/**
 * Owns the single PhoneCore and the network lifetime.
 *
 * Every PhoneCore method is a blocking call: use them from Dispatchers.IO ([phone] itself blocks the
 * first time). Who wants the network says so with [hold] and lets go with [release]; the endpoint runs
 * while at least one owner holds it ("service", "ui", "worker", "pairing").
 */
@OptIn(ExperimentalCoroutinesApi::class)
class Core(private val app: Application) {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    private val openLock = Any()
    @Volatile private var phone: PhoneCore? = null

    /** Core events for the screens. Events carry ids, not rows: re-read what is on screen. */
    private val _events = MutableSharedFlow<FfiEvent>(extraBufferCapacity = 256, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    val events: SharedFlow<FfiEvent> = _events

    private val _status = MutableStateFlow<SyncStatus?>(null)
    val status: StateFlow<SyncStatus?> = _status

    /** Newest background errors (a failed download, a blocked non-local address, a log file problem), newest last. */
    private val _errors = MutableStateFlow<List<String>>(emptyList())
    val errors: StateFlow<List<String>> = _errors

    /** Folder id chosen on the viewfinder chip for the NEXT shot. The camera clears it after a shot. Share uses it too. */
    val nextShotFolder = MutableStateFlow<String?>(null)

    /** True while SnapService runs in this process. A new process starts with false. */
    @Volatile var serviceRunning = false

    private inner class Listener : EventListener {
        override fun onEvent(event: FfiEvent) {
            // Runs on a Rust runtime thread: record and return, never call back into the core.
            when (event) {
                is FfiEvent.Sync -> _status.value = event.status
                is FfiEvent.Error -> reportError("${event.kind}: ${event.message}")
                else -> {}
            }
            _events.tryEmit(event)
        }
    }

    fun reportError(text: String) {
        KLog.log(app, "ERROR $text")
        _errors.value = (_errors.value + text).takeLast(20)
    }

    fun clearErrors() {
        _errors.value = emptyList()
    }

    /** Opens the core on first use. Blocking. Throws UpgradeStoppedException, KeyVaultException or SnapException; the caller shows the message. */
    fun phone(): PhoneCore {
        phone?.let { return it }
        synchronized(openLock) {
            phone?.let { return it }
            // The old library could not be moved to its new name: opening now would create an empty tugboat.db and the photos would look gone.
            LegacyMigration.stopMessage?.let { throw UpgradeStoppedException(it) }
            val secret = KeyVault.loadOrCreate(app)
            val p = PhoneCore.open(
                PhoneConfig(
                    dataDir = app.filesDir.path,
                    deviceName = Build.MODEL,
                    appVersion = BuildConfig.VERSION_NAME,
                    secretKey = secret,
                    bindAddr = null,
                ),
                Listener(),
            )
            phone = p
            _status.value = p.status()
            KLog.log(app, "core open, device ${p.deviceId().take(8)}")
            // An update that renamed old files (LegacyMigration) leaves a note: now the history can take it.
            LegacyMigration.flushActivity(app) { p.logUpgrade(it) }
            return p
        }
    }

    /** The core if it is already open, else null. Never blocks. */
    fun phoneIfOpen(): PhoneCore? = phone

    // ---- network lifetime ----

    // Start/stop calls block, so they run one at a time, in order, on one thread.
    private val linkScope = CoroutineScope(SupervisorJob() + Dispatchers.IO.limitedParallelism(1))
    private val holds = mutableSetOf<String>()
    private val netUp = MutableStateFlow(false)

    /** The returned Job completes once the network is up (or has failed: the error is in [errors]). */
    fun hold(owner: String): Job = linkScope.launch {
        holds += owner
        applyLink()
    }

    fun release(owner: String): Job = linkScope.launch {
        holds -= owner
        applyLink()
    }

    /** Stop and start the endpoint again (the local-network permission was just granted, for example). */
    fun restartLink() {
        linkScope.launch {
            if (netUp.value) {
                phone?.stopNetwork()
                netUp.value = false
            }
            applyLink()
        }
    }

    // Only called on linkScope.
    private fun applyLink() {
        try {
            val p = phone()
            if (holds.isNotEmpty() && !netUp.value) {
                // mDNS without the local-network permission only floods the log with EPERM, so leave it off.
                p.startNetwork(Perms.localNetworkAllowed(app))
                netUp.value = true
                KLog.log(app, "network up, holders=$holds")
            } else if (holds.isEmpty() && netUp.value) {
                p.stopNetwork()
                netUp.value = false
                KLog.log(app, "network down")
            }
        } catch (e: UpgradeStoppedException) {
            reportError(e.message ?: "upgrade stopped") // the screen already shows the blocker; no network without a library
        } catch (e: KeyVaultException) {
            reportError(e.message ?: "key error")
        } catch (e: SnapException) {
            reportError("network: ${e.message}")
        }
    }

    // ---- multicast lock bursts ----

    init {
        // The multicast lock keeps the Wi-Fi radio awake for every multicast packet, so it is held only
        // while the PC cannot be reached (that is when mDNS lookups run), in bursts: 30 s on, 2 min off.
        scope.launch {
            combine(netUp, status) { up, st -> up && (st?.state == SyncState.OFFLINE || st?.state == SyncState.CONNECTING) }
                .distinctUntilChanged()
                .collectLatest { searching -> if (searching) multicastBursts() }
        }
    }

    private suspend fun multicastBursts() {
        val lock = app.getSystemService(WifiManager::class.java).createMulticastLock("localtugboat-mdns")
        lock.setReferenceCounted(false)
        try {
            while (true) {
                lock.acquire()
                delay(BURST_MS)
                lock.release()
                delay(PAUSE_MS)
            }
        } finally {
            // Cancelled when the PC is found or the link goes down: never leave the lock held.
            if (lock.isHeld) lock.release()
        }
    }

    companion object {
        const val BURST_MS = 30_000L
        const val PAUSE_MS = 120_000L
    }
}
