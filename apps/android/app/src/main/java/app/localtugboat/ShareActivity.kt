package app.localtugboat

import android.content.Intent
import android.net.Uri
import androidx.core.content.IntentCompat
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.compose.setContent
import androidx.lifecycle.lifecycleScope
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Process-wide count of share imports running. Every ShareActivity instance finishes when it reaches zero. */
object ShareRuns {
    val active = MutableStateFlow(0)

    /** Tokens of the imports this process started: an activity restored with a token not in here lost its import to a process death. */
    private val started: MutableSet<String> = java.util.concurrent.ConcurrentHashMap.newKeySet()

    fun begin(): String = java.util.UUID.randomUUID().toString().also { started += it }

    /** True when [token] belongs to an import that was started, but not by this process. No token = nothing was started. */
    fun interrupted(token: String?): Boolean = token != null && token !in started
}

/**
 * Share > Tugboat (ACTION_SEND / ACTION_SEND_MULTIPLE for any image or video type). Stays open until the
 * copy is done: the sender's temporary read grant lives only as long as this activity.
 *
 * The import runs on the process scope and ends the activity through [ShareRuns], not through `this`: if the
 * system recreates the activity (a configuration change, or the process died and the task is restored) the new
 * instance still finishes, and never starts the import a second time.
 */
class ShareActivity : ComponentActivity() {
    private var token: String? = null

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putString(KEY_TOKEN, token)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        token = savedInstanceState?.getString(KEY_TOKEN)
        // Back would finish this activity and withdraw the sender's read grant while the import still opens later files.
        // Home still works: the activity is only stopped, the grant stays.
        val keepOpen = object : OnBackPressedCallback(false) {
            override fun handleOnBackPressed() {}
        }
        onBackPressedDispatcher.addCallback(this, keepOpen)
        setContent {
            SnapTheme {
                Column(Modifier.padding(24.dp), verticalArrangement = Arrangement.spacedBy(12.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                    CircularProgressIndicator()
                    Text("Adding to Tugboat")
                }
            }
        }
        // A recreated activity must not import the same files twice: it only waits for the running import (none = leaves).
        if (savedInstanceState != null && ShareRuns.interrupted(token)) {
            // Restored after the process died mid-import: the files not yet added are gone with it. Say so, add nothing twice.
            Toast.makeText(this, "IMPORT INTERRUPTED - check the gallery, share the rest again", Toast.LENGTH_LONG).show()
            finish()
            return
        }
        if (savedInstanceState == null) {
            val uris = sharedUris(intent)
            if (uris.isEmpty()) {
                Toast.makeText(this, "Nothing to add", Toast.LENGTH_LONG).show()
                finish()
                return
            }
            val app = applicationContext
            token = ShareRuns.begin()
            ShareRuns.active.update { it + 1 }
            // Process scope, not the activity's: a rotation or a back press must not stop a half-copied video.
            app.core.scope.launch {
                val text = try {
                    ShareImporter.run(app, uris).summary()
                } catch (e: Exception) {
                    app.core.reportError("share failed: ${e.message}")
                    "Could not add: ${e.message}"
                }
                withContext(Dispatchers.Main) {
                    try {
                        Toast.makeText(app, text, Toast.LENGTH_LONG).show()
                    } finally {
                        ShareRuns.active.update { it - 1 }
                    }
                }
            }
        }
        lifecycleScope.launch { ShareRuns.active.collect { keepOpen.isEnabled = it > 0 } }
        lifecycleScope.launch { ShareRuns.active.first { it == 0 }; finish() }
    }

    private companion object {
        const val KEY_TOKEN = "importToken"
    }

    private fun sharedUris(i: Intent): List<Uri> = when (i.action) {
        Intent.ACTION_SEND -> listOfNotNull(IntentCompat.getParcelableExtra(i, Intent.EXTRA_STREAM, Uri::class.java))
        Intent.ACTION_SEND_MULTIPLE -> IntentCompat.getParcelableArrayListExtra(i, Intent.EXTRA_STREAM, Uri::class.java).orEmpty()
        else -> emptyList()
    }
}
