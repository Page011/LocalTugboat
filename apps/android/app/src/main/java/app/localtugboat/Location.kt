package app.localtugboat

import android.annotation.SuppressLint
import android.content.Context
import android.location.Location
import android.location.LocationListener
import android.location.LocationManager
import android.os.Looper
import android.os.SystemClock
import uniffi.tugboat_ffi.LocationFix

/**
 * Location for place rules. Runs only while the camera screen is open (call [start] in onStart or when
 * the screen appears, [stop] when it goes away): while-in-use permission, never background location.
 * Uses the platform fused provider, so no Google Play Services library is linked.
 */
class LocationTracker(private val c: Context) : LocationListener {
    private val lm = c.getSystemService(LocationManager::class.java)
    @Volatile private var last: Location? = null
    private var running = false

    /** Returns an error text when updates cannot start (no permission, no provider), else null. */
    @SuppressLint("MissingPermission") // checked through Perms.location just below
    fun start(): String? {
        if (running) return null
        if (Perms.location(c) == LocationAccess.NONE) return "Location permission is off"
        val provider = LocationManager.FUSED_PROVIDER
        if (!lm.hasProvider(provider)) return "This phone has no fused location provider"
        return try {
            last = lm.getLastKnownLocation(provider)
            lm.requestLocationUpdates(provider, UPDATE_MS, 0f, this, Looper.getMainLooper())
            running = true
            null
        } catch (e: SecurityException) {
            "Location permission was withdrawn"
        }
    }

    fun stop() {
        if (!running) return
        lm.removeUpdates(this)
        running = false
    }

    /**
     * The newest fix with its age, or null if there is none or it has no accuracy. The core applies the
     * rules (under 2 minutes old, 100 m or better), so a bad fix is passed on and refused there.
     */
    fun current(): LocationFix? {
        val l = last ?: return null
        if (!l.hasAccuracy()) return null
        val ageMs = (SystemClock.elapsedRealtimeNanos() - l.elapsedRealtimeNanos) / 1_000_000
        return LocationFix(l.latitude, l.longitude, l.accuracy.toDouble(), maxOf(0L, ageMs))
    }

    override fun onLocationChanged(location: Location) {
        last = location
    }

    private companion object {
        const val UPDATE_MS = 5_000L
    }
}
