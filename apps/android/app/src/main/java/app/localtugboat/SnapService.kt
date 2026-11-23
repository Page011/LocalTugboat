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
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import uniffi.tugboat_ffi.FfiEvent

/**
 * Foreground service (connectedDevice): keeps the network up while a PC is linked, writes the
 * heartbeat and alerts on sync problems. The core itself lives in [Core], not here, so
 * the activity and the worker share it and a restart of this service never reopens the database.
 */
class SnapService : Service() {
    private var job: Job? = null
    private var netCb: ConnectivityManager.NetworkCallback? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // startForeground first, always: the 5 s rule applies even when we are about to stop again.
        Notif.ensureChannels(this)
        startForeground(Notif.ID, Notif.running(this), ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
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
                launch { alertOnProblems() }
                launch {
                    // One PC unlinked while others stay: the service keeps running for them.
                    core.events.collect { if (it is FfiEvent.Unpaired && !stillLinked()) stop(this@SnapService) }
                }
            }
        }
        // After a system kill the system restarts us; a force-stop is only undone by opening the app.
        return START_STICKY
    }

    /** Asked of the core after an unlink. A failed read keeps the service up; the error is reported. */
    private fun stillLinked(): Boolean = try {
        UiLogic.linked(core.phone().status().devices)
    } catch (e: Exception) {
        core.reportError("unlink check: ${e.message}")
        true
    }

    /**
     * Alerts once a problem has lasted [ALERT_HOLD_MS], so one that clears by itself (a storage check right after a
     * download) never sounds. A new problem cancels the wait for the old one; a solved one takes the alert away.
     */
    private suspend fun alertOnProblems() {
        // No status yet means the core is still opening: not known, so it neither raises nor clears an alert.
        core.status.filterNotNull().map { UiLogic.alert(it) }.distinctUntilChanged().collectLatest { text ->
            if (text == null) {
                Notif.clearProblem(this@SnapService)
            } else {
                delay(ALERT_HOLD_MS)
                Notif.problem(this@SnapService, text)
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
            // Unpaired or background sync turned off: nothing is watching the problem any more.
            Notif.clearProblem(this)
            stopWanted = false
        }
        core.serviceRunning = false
        core.release(HOLD)
        KLog.log(this, "service destroyed")
        super.onDestroy()
    }

    companion object {
        private const val HOLD = "service"
        private const val ALERT_HOLD_MS = 30_000L

        /** Set by [stop]: the next destroy is on purpose, so its quiet time is not a failure. */
        @Volatile private var stopWanted = false

        /**
         * Returns an error text if the system refused to start the service, else null.
         *
         * A refusal is always in the log. It becomes a banner only when no start has succeeded within
         * [StartRefusal.GRACE_MS]: Android can refuse one start (a broadcast delivered after a force-stop, a worker with
         * no foreground standing) while the next one, from the opened app, works. A banner for the first would stay on
         * screen although the service is running.
         */
        fun start(c: Context, why: String = "?"): String? = try {
            c.applicationContext.startForegroundService(Intent(c.applicationContext, SnapService::class.java))
            null
        } catch (e: Exception) {
            val m = "service start refused ($why): ${e.javaClass.simpleName}: ${e.message}"
            KLog.log(c, m)
            val core = c.core
            StartRefusal.watch(core.scope, StartRefusal.GRACE_MS, { core.serviceRunning }, { core.reportError(it) }, m)
            m
        }

        /** Deliberate stop: setting turned off, or the last PC unlinked. */
        fun stop(c: Context) {
            if (!c.core.serviceRunning) return
            stopWanted = true
            c.applicationContext.stopService(Intent(c.applicationContext, SnapService::class.java))
        }
    }
}

/** The rule for a refused service start: tell the user only if the service is still not running after a grace period. */
object StartRefusal {
    const val GRACE_MS = 10_000L

    /** After [graceMs], calls [report] with [text] unless [running] says a start has worked since. */
    fun watch(scope: kotlinx.coroutines.CoroutineScope, graceMs: Long, running: () -> Boolean, report: (String) -> Unit, text: String): Job =
        scope.launch {
            delay(graceMs)
            if (!running()) report(text)
        }
}
