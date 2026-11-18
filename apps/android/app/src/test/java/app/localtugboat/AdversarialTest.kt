package app.localtugboat

import java.io.File
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Verifier-stage findings written as tests. They read the sources and the manifest because the
 * paths involved (camera hand-off, deep-link guard, library manifests) need an Android runtime to
 * run; each would pass once the finding is fixed.
 */
class AdversarialTest {
    private val main: File = listOf(File("src/main"), File("app/src/main")).first { it.isDirectory }
    private val kotlin = File(main, "java/app/localtugboat")

    private fun sources() = kotlin.listFiles { f -> f.extension == "kt" }.orEmpty().associate { it.name to it.readText() }

    /**
     * CameraScreen.handOff and Capture.add promise "the file stays in shots/ and Capture.recover retries it:
     * nothing is lost". Nothing calls recover(), so a shot whose hand-off failed (disk full, folder deleted,
     * core not open) or whose process died between CameraX saving it and the core taking it stays in
     * filesDir/shots for ever: not in the gallery, never synced.
     */
    @Test
    fun failedShotsAreRetriedSomewhere() {
        val callers = sources().filter { (name, text) -> name != "Import.kt" && Regex("""Capture\.recover\(""").containsMatchIn(text) }
        assertTrue("Capture.recover is never called: orphaned shots in filesDir/shots are never retried", callers.isNotEmpty())
    }

    /**
     * onDeepLink refuses a pairing link while paired by reading meta.value.paired. A link that starts the app
     * (or a new MainActivity, which gets a new view model) arrives before refresh() has opened the core, so
     * paired is still null and the "Pair with this PC?" dialog shows over the paired app. Tapping Pair then
     * re-pairs the phone with whatever PC is in the link, and the paired app never shows the 6-digit code.
     */
    @Test
    fun deepLinkGuardDoesNotTrustUnloadedState() {
        val vm = sources().getValue("SnapViewModel.kt")
        val body = vm.substringAfter("fun onDeepLink(").substringBefore("fun dismissDeepLink(")
        val readsFreshState = body.contains("pairedPc()") || body.contains(".loaded")
        assertTrue("onDeepLink/confirmDeepLink decide 'already paired' from state that is not loaded yet on a cold start", readsFreshState)
    }

    /**
     * PLAN.md security: "no telemetry, no outbound internet traffic". ML Kit barcode-scanning 17.3.0 pulls in
     * com.google.android.datatransport (CCT backend), which ML Kit's own data-disclosure page says sends
     * diagnostics and usage analytics to Google over HTTPS. Nothing in our manifest removes it.
     */
    @Test
    fun mlKitTelemetryBackendIsRemoved() {
        val manifest = File(main, "AndroidManifest.xml").readText()
        val removed = Regex("""TransportBackendDiscovery"[^>]*tools:node="remove"""").containsMatchIn(manifest)
        assertTrue("ML Kit's datatransport (Clearcut) backend is merged into the app: outbound telemetry to Google", removed)
    }
}
