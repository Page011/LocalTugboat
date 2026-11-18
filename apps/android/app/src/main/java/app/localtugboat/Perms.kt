package app.localtugboat

import android.Manifest
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.core.net.toUri

enum class LocationAccess { NONE, APPROXIMATE, PRECISE }

/** One snapshot of every permission state the screens show a message for. */
data class PermState(
    val camera: Boolean,
    val microphone: Boolean,
    val location: LocationAccess,
    /** False when the permission is denied OR the user switched the app's notifications off. */
    val notifications: Boolean,
    /** True below API 37, where the permission does not exist. */
    val localNetwork: Boolean,
    val batteryExempt: Boolean,
)

object Perms {
    /** API 37+ blocks all local network traffic without it: UDP fails with EPERM, TCP times out. */
    const val LOCAL_NETWORK = "android.permission.ACCESS_LOCAL_NETWORK"

    private fun granted(c: Context, p: String) = c.checkSelfPermission(p) == PackageManager.PERMISSION_GRANTED

    fun localNetworkAllowed(c: Context) = Build.VERSION.SDK_INT < 37 || granted(c, LOCAL_NETWORK)

    fun notificationsAllowed(c: Context): Boolean =
        (Build.VERSION.SDK_INT < 33 || granted(c, Manifest.permission.POST_NOTIFICATIONS)) &&
            c.getSystemService(NotificationManager::class.java).areNotificationsEnabled()

    fun cameraAllowed(c: Context) = granted(c, Manifest.permission.CAMERA)

    fun location(c: Context) = when {
        granted(c, Manifest.permission.ACCESS_FINE_LOCATION) -> LocationAccess.PRECISE
        granted(c, Manifest.permission.ACCESS_COARSE_LOCATION) -> LocationAccess.APPROXIMATE
        else -> LocationAccess.NONE
    }

    fun batteryExempt(c: Context): Boolean =
        c.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(c.packageName)

    /** For a permission the system will not ask for again: the user has to switch it on here. */
    fun appSettingsIntent(c: Context): Intent =
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, "package:${c.packageName}".toUri()).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)

    fun snapshot(c: Context) = PermState(
        camera = cameraAllowed(c),
        microphone = granted(c, Manifest.permission.RECORD_AUDIO),
        location = location(c),
        notifications = notificationsAllowed(c),
        localNetwork = localNetworkAllowed(c),
        batteryExempt = batteryExempt(c),
    )
}
