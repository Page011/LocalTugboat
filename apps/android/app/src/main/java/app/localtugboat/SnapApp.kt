package app.localtugboat

import android.app.Application
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import java.util.concurrent.TimeUnit

class SnapApp : Application() {
    /** Created here, opened lazily: the core opens on the first caller that needs it, on an IO thread. */
    val core by lazy { Core(this) }

    override fun onCreate() {
        super.onCreate()
        // First, before anything opens the core or reads Prefs: carry data over from the 0.1.0 file names.
        LegacyMigration.run(this)
        Notif.ensureChannels(this)
        // UPDATE, not KEEP: 0.1.0 scheduled "catchup" under the Kotlin class name app.snapferry.CatchUpWorker, which no
        // longer exists. WorkManager stores that class name, so KEEP would leave a worker it can never create and the
        // 15 minute catch-up would stay off. UPDATE swaps the spec in place (and keeps the schedule when nothing changed).
        WorkManager.getInstance(this).enqueueUniquePeriodicWork(
            CatchUpWorker.NAME,
            ExistingPeriodicWorkPolicy.UPDATE,
            PeriodicWorkRequestBuilder<CatchUpWorker>(15, TimeUnit.MINUTES).build(),
        )
    }
}
