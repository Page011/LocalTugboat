package app.localtugboat

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/**
 * Two channels. "running" carries the notification Android requires while the sync service runs: silent, no badge,
 * one fixed line, posted once and never updated. "problems" alerts once when sync needs the user ([UiLogic.alert]).
 */
object Notif {
    private const val RUNNING = "running"
    private const val PROBLEMS = "problems"
    const val ID = 1
    private const val PROBLEM_ID = 2

    fun ensureChannels(c: Context) {
        val nm = c.getSystemService(NotificationManager::class.java)
        // 0.2.0 and older posted a live status on "link" every 15 s. A channel's badge setting cannot change after it is
        // created, so it is replaced. Deleting fails while a foreground service still shows on it; the next start retries.
        runCatching { nm.deleteNotificationChannel("link") }
        // LOW, not MIN: for a foreground service on a MIN channel Android posts its own "running in the background" notice.
        nm.createNotificationChannel(NotificationChannel(RUNNING, "Sync running", NotificationManager.IMPORTANCE_LOW).apply { setShowBadge(false) })
        nm.createNotificationChannel(NotificationChannel(PROBLEMS, "Sync problems", NotificationManager.IMPORTANCE_DEFAULT))
    }

    fun running(c: Context): Notification = Notification.Builder(c, RUNNING)
        .setSmallIcon(R.drawable.ic_stat_sync)
        .setContentTitle("Tugboat")
        .setContentText("Sync on")
        .setContentIntent(open(c))
        .setOngoing(true)
        .setShowWhen(false)
        .build()

    /** Sounds once; posting the same problem again while it is still shown stays silent. */
    fun problem(c: Context, text: String) {
        val n = Notification.Builder(c, PROBLEMS)
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setContentTitle("Tugboat")
            .setContentText(text)
            .setContentIntent(open(c))
            .setOnlyAlertOnce(true)
            .setAutoCancel(true)
            .build()
        c.getSystemService(NotificationManager::class.java).notify(PROBLEM_ID, n)
    }

    fun clearProblem(c: Context) = c.getSystemService(NotificationManager::class.java).cancel(PROBLEM_ID)

    private fun open(c: Context) = PendingIntent.getActivity(c, 0, Intent(c, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
}
