package app.localtugboat

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/** The ongoing sync notification. Low importance: no sound, no heads-up. The app never relies on it alone. */
object Notif {
    private const val CHANNEL = "link"
    const val ID = 1

    fun ensureChannel(c: Context) {
        c.getSystemService(NotificationManager::class.java)
            .createNotificationChannel(NotificationChannel(CHANNEL, "Tugboat link", NotificationManager.IMPORTANCE_LOW))
    }

    fun build(c: Context, text: String): Notification {
        val open = PendingIntent.getActivity(c, 0, Intent(c, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)
        return Notification.Builder(c, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setContentTitle("Tugboat")
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE)
            .build()
    }

    fun update(c: Context, text: String) {
        c.getSystemService(NotificationManager::class.java).notify(ID, build(c, text))
    }
}
