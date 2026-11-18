package app.localtugboat

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings
import androidx.core.net.toUri

/**
 * The [Fix] flow after "Tugboat was stopped": step 1 opens the system's battery optimisation list, where the
 * user sets Tugboat to "Not optimized" (the only thing that lets a worker restart the service on Android 12+);
 * step 2 opens the vendor's "never sleeping apps" screen, falling back to the app's own settings page.
 * The UI runs the steps in order, launching each with startActivity.
 *
 * Step 1 is the list, not the one-tap request dialog (ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS): Play bans that
 * request for apps whose core function is not hurt by Doze, and a PC sync app is the "Not acceptable" row of the
 * Doze use-case table. The list is allowed for every app and needs no permission.
 */
object BatteryFix {
    /** Step 1. Null when the app is already exempt. */
    fun exemptionIntent(c: Context): Intent? =
        if (Perms.batteryExempt(c)) null
        else Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS)

    // ponytail: Samsung component names are not public API. These come from other apps' working deep
    // links and were NOT verified on a real One UI phone; the app-details fallback always works.
    private val samsung = listOf(
        ComponentName("com.samsung.android.lool", "com.samsung.android.sm.battery.ui.BatteryActivity"),
        ComponentName("com.samsung.android.lool", "com.samsung.android.sm.ui.battery.BatteryActivity"),
        ComponentName("com.samsung.android.sm_cn", "com.samsung.android.sm.ui.battery.BatteryActivity"),
    )

    /** Step 2. Starts the first screen that opens and returns a short name of it for the log. */
    fun openVendorScreen(c: Context): String {
        if (Build.MANUFACTURER.equals("samsung", ignoreCase = true)) {
            for (cn in samsung) {
                try {
                    c.startActivity(Intent().setComponent(cn).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                    return cn.className
                } catch (e: Exception) {
                    // Not on this firmware, or not exported: try the next one.
                }
            }
        }
        c.startActivity(
            Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, "package:${c.packageName}".toUri()).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        )
        return "app details"
    }
}
