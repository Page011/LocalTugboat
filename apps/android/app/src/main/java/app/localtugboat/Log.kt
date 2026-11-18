package app.localtugboat

import android.content.Context
import android.content.SharedPreferences
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Small per-app flags that are not core data (banner dismissals, one-time prompts). */
object Prefs {
    fun of(c: Context): SharedPreferences = c.applicationContext.getSharedPreferences("tugboat", Context.MODE_PRIVATE)
}

/**
 * Kotlin-side log (filesDir/kotlin.log, 512 KB, newest half kept), mirrored to logcat tag Tugboat.
 * The Rust side writes its own rotating log; "Export log" shares both. No secrets, no file names of
 * user photos: only events and error texts.
 */
object KLog {
    private const val MAX = 512 * 1024
    private val fmt = SimpleDateFormat("yyyy-MM-dd HH:mm:ss.SSS", Locale.US)

    fun file(c: Context) = File(c.applicationContext.filesDir, "kotlin.log")

    @Synchronized
    fun log(c: Context, msg: String) {
        android.util.Log.i("Tugboat", msg)
        try {
            val f = file(c)
            f.appendText("${fmt.format(Date())} $msg\n")
            if (f.length() > MAX) f.writeText(f.readText().takeLast(MAX / 2))
        } catch (e: Exception) {
            android.util.Log.e("Tugboat", "log write failed: $e")
        }
    }
}
