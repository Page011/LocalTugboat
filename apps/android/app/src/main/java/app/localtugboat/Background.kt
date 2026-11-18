package app.localtugboat

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.work.CoroutineWorker
import androidx.work.WorkerParameters
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import uniffi.tugboat_ffi.SyncState

/** The one rule for "should the foreground service run": paired, and the setting is on. */
object ServiceGate {
    /** Blocking (opens the core). Paired and the setting on. False when that cannot be read (reported). */
    fun wanted(c: Context, why: String): Boolean {
        val core = c.applicationContext.core
        return try {
            val p = core.phone()
            p.pairedPc() != null && p.settings().stayConnected
        } catch (e: Exception) {
            core.reportError("start check ($why) failed: ${e.message}")
            false
        }
    }

    /** Blocking (opens the core). Starts the service if it is wanted and returns whether it was. */
    fun startIfWanted(c: Context, why: String): Boolean {
        val core = c.applicationContext.core
        val wanted = wanted(c, why)
        KLog.log(c, "start check ($why): wanted=$wanted running=${core.serviceRunning}")
        if (wanted && !core.serviceRunning) SnapService.start(c)
        return wanted
    }
}

/**
 * BOOT_COMPLETED and MY_PACKAGE_REPLACED: both may start a foreground service from the background.
 * After a force-stop, Android 15+ also delivers BOOT_COMPLETED once the app is opened again, so this
 * can run on a phone that did not reboot: harmless, the gate checks `serviceRunning`.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val pending = goAsync()
        val app = context.applicationContext
        app.core.scope.launch {
            try {
                ServiceGate.startIfWanted(app, intent.action ?: "?")
            } finally {
                pending.finish()
            }
        }
    }
}

/**
 * Every 15 minutes: a catch-up sync when the service is not running, and a service restart when the
 * app is exempt from battery optimisation (Android 12+ refuses a background start otherwise).
 */
class CatchUpWorker(c: Context, p: WorkerParameters) : CoroutineWorker(c, p) {
    // The core calls block: keep them off the default dispatcher.
    override suspend fun doWork(): Result = withContext(Dispatchers.IO) { catchUp() }

    private suspend fun catchUp(): Result {
        val app = applicationContext
        val core = app.core
        try {
            val p = core.phone()
            val recovered = Capture.recover(app)
            if (recovered > 0) KLog.log(app, "worker: took $recovered shot(s) left from an earlier failed hand-off")
            if (p.pairedPc() == null) return Result.success()
            if (p.settings().stayConnected && !core.serviceRunning) {
                if (Perms.batteryExempt(app)) {
                    KLog.log(app, "worker: service not running, restarting it")
                    SnapService.start(app)
                } else {
                    KLog.log(app, "worker: service not running and not exempt from battery optimisation, cannot restart it")
                }
            }
            if (core.serviceRunning) return Result.success() // the service is syncing already
            if (!Perms.localNetworkAllowed(app)) {
                core.reportError("catch-up skipped: local network blocked")
                return Result.success()
            }
            val startedAt = System.currentTimeMillis()
            core.hold("worker")
            try {
                p.reconnectNow()
                val done = withTimeoutOrNull(SYNC_WAIT_MS) {
                    core.status.first {
                        // A fresh sync, not the stale status left from the last session.
                        it != null && it.state == SyncState.SYNCED && it.downloading == 0u && (it.lastSyncedAt ?: 0L) >= startedAt - 1000
                    }
                }
                KLog.log(app, "worker: catch-up ${if (done != null) "synced" else "timed out, PC not reachable"}")
            } finally {
                core.release("worker")
            }
        } catch (e: CancellationException) {
            throw e // WorkManager cancelled this run (an updated schedule, a stop): not a failure to report
        } catch (e: Exception) {
            // Reported, then success: a retry loop would only drain the battery on a phone that has no PC nearby.
            core.reportError("catch-up failed: ${e.message}")
        }
        return Result.success()
    }

    companion object {
        const val NAME = "catchup"
        private const val SYNC_WAIT_MS = 90_000L
    }
}
