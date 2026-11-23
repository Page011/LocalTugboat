package app.localtugboat

import java.io.File
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Checks that read the sources and the manifest, because the paths involved (camera hand-off,
 * deep-link guard, library manifests) need an Android runtime to run.
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
     * A deep link is accepted while PCs are linked, and the Link button goes to the Link a PC screen first: the main
     * screens never show the code check. A link that starts the app arrives before refresh() has opened the core, so
     * whether a PC is linked has to come from the core's own device list, not the cached state.
     */
    @Test
    fun deepLinkOpensLinkScreenFromCoreState() {
        val vm = sources().getValue("SnapViewModel.kt")
        val body = vm.substringAfter("fun confirmDeepLink(").substringBefore("fun dismissDeepLink(")
        assertTrue("the linked check reads state that is not loaded yet on a cold start", body.contains("UiLogic.linked(core.phone().status().devices)"))
        assertTrue("the Link a PC screen must be up before pairing starts", body.indexOf("openAddDevice()") in 0 until body.indexOf("pair(d.payload)"))
    }

    /**
     * Security rule: no telemetry and no outbound internet traffic. ML Kit barcode-scanning 17.3.0 pulls in
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
