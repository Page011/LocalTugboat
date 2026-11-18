package app.localtugboat

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.os.IBinder
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import uniffi.tugboat_ffi.FfiEvent

/**
 * Foreground service (connectedDevice): keeps the network up while the phone is paired, writes the
 * heartbeat and keeps the notification text current. The core itself lives in [Core], not here, so
 * the activity and the worker share it and a restart of this service never reopens the database.
 */
class SnapService : Service() {
    private var job: Job? = null
    private var netCb: ConnectivityManager.NetworkCallback? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // startForeground first, always: the 5 s rule applies even when we are about to stop again.
        Notif.ensureChannel(this)
        startForeground(
            Notif.ID,
            Notif.build(this, core.status.value?.let { Texts.notification(it, System.currentTimeMillis()) } ?: "Starting..."),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
        )
        if (job == null) {
            KLog.log(this, "service started")
            core.serviceRunning = true
            core.hold(HOLD)
            watchWifi()
            job = core.scope.launch {
                // A start that was still queued when the setting went off or the phone was unpaired: stop() saw no running
                // service and did nothing, so the check is repeated here (after startForeground, which must come first).
                if (!ServiceGate.wanted(this@SnapService, "service start")) {
                    // Free the flag first, then look again: a setting turned back on in between either sees the flag free
                    // (and starts us anew) or is seen by this second look.
                    core.serviceRunning = false
                    if (!ServiceGate.wanted(this@SnapService, "service start, second look")) {
                        KLog.log(this@SnapService, "service not wanted any more, stopping")
                        stopWanted = true
                        // stopSelfResult, not stopSelf: false means a newer start arrived (the setting went back on while we
                        // looked). That start found this job alive and did nothing, so this job must carry on instead of stopping.
                        if (stopSelfResult(startId)) return@launch
                        stopWanted = false
                        KLog.log(this@SnapService, "a newer start arrived, staying up")
                    }
                    core.serviceRunning = true
                }
                launch { while (true) { Heartbeat.beat(this@SnapService); delay(HeartbeatLog.BEAT_MS) } }
                launch { keepNotificationCurrent() }
                launch {
                    core.events.collect { if (it is FfiEvent.Unpaired) stop(this@SnapService) }
                }
            }
        }
        // After a system kill the system restarts us; a force-stop is only undone by opening the app.
        return START_STICKY
    }

    private suspend fun keepNotificationCurrent() {
        // collectLatest: a new status cancels the old 15 s refresh loop and starts a fresh one.
        core.status.collectLatest { st ->
            while (true) {
                val text = if (st == null) core.errors.value.lastOrNull() ?: "Starting..." else Texts.notification(st, System.currentTimeMillis())
                Notif.update(this@SnapService, text)
                delay(NOTIF_REFRESH_MS)
            }
        }
    }

    /** A Wi-Fi change (new access point, new address) means the cached PC address may be stale: dial now. */
    private fun watchWifi() {
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                core.phoneIfOpen()?.reconnectNow()
            }
        }
        netCb = cb
        try {
            getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(cb)
        } catch (e: Exception) {
            netCb = null
            core.reportError("wifi watcher: ${e.message}")
        }
    }

    override fun onDestroy() {
        netCb?.let { runCatching { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) } }
        netCb = null
        // Join so no late beat lands after the stop marker and shows up later as a false gap.
        runBlocking { job?.cancelAndJoin() }
        job = null
        if (stopWanted) {
            Heartbeat.markStopped(this)
            stopWanted = false
        }
        core.serviceRunning = false
        core.release(HOLD)
        KLog.log(this, "service destroyed")
        super.onDestroy()
    }

    companion object {
        private const val HOLD = "service"
        private const val NOTIF_REFRESH_MS = 15_000L

        /** Set by [stop]: the next destroy is on purpose, so its quiet time is not a failure. */
        @Volatile private var stopWanted = false

        /** Returns an error text if the system refused to start the service, else null. */
        fun start(c: Context): String? = try {
            c.applicationContext.startForegroundService(Intent(c.applicationContext, SnapService::class.java))
            null
        } catch (e: Exception) {
            val m = "service start refused: ${e.javaClass.simpleName}: ${e.message}"
            c.core.reportError(m)
            m
        }

        /** Deliberate stop: setting turned off, or unpaired. */
        fun stop(c: Context) {
            if (!c.core.serviceRunning) return
            stopWanted = true
            c.applicationContext.stopService(Intent(c.applicationContext, SnapService::class.java))
        }
    }
}
