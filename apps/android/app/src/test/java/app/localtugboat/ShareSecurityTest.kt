package app.localtugboat

import java.io.File
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The any-type share target against hostile content URIs, the FileProvider's reach, and the names other apps are shown.
 */
class ShareSecurityTest {
    private val main: File = listOf(File("src/main"), File("app/src/main")).first { it.isDirectory }
    private val own = "app.snapferry.fileprovider"

    /**
     * ContentResolver resolves "content://0@app.snapferry.fileprovider/items/x" to OUR provider for user 0
     * (ContentProvider.getAuthorityWithoutUserId strips the "<userId>@" prefix), and our own process may read it.
     * A hostile app that shares such a URI makes Tugboat copy its own held files or its exported log as a new
     * item: the "our own provider is never a source" rule must hold for the user-prefixed spelling too.
     */
    @Test
    fun own_provider_with_a_user_id_prefix_is_refused() {
        for (authority in listOf("0@$own", "10@$own", "0@APP.SNAPFERRY.FILEPROVIDER")) {
            assertFalse("content://$authority/... reaches our own FileProvider", ShareNames.acceptableUri("content", authority, own))
        }
    }

    /** Scheme spellings ContentResolver does not treat as content:// must not pass either. */
    @Test
    fun only_the_exact_content_scheme_passes() {
        for (scheme in listOf("file", "FILE", "android.resource", "CONTENT", "", "http", "https")) {
            assertFalse("scheme $scheme accepted", ShareNames.acceptableUri(scheme, "media", own))
        }
        assertTrue(ShareNames.acceptableUri("content", "media", own))
    }

    /**
     * The name another app sees (DISPLAY_NAME of an opened or shared file) comes from the item's record name. A
     * bidi override makes "invoice<RLO>fdp.exe" read "invoiceexe.pdf" in the receiving app, so providerName must
     * remove the bidi format characters as well as the control characters.
     */
    @Test
    fun the_name_shown_to_other_apps_has_no_bidi_override() {
        for (raw in listOf("invoice\u202Efdp.exe", "a\u2066b.apk", "x\u200F.pdf")) {
            val shown = FileRules.providerName(raw) ?: continue
            assertFalse("other apps are shown ${shown.map { "%04X".format(it.code) }}", shown.any { isBidiControl(it) })
        }
    }

    private fun isBidiControl(ch: Char) =
        ch in '\u202A'..'\u202E' || ch in '\u2066'..'\u2069' || ch == '\u200E' || ch == '\u200F' || ch == '\u061C'

    /** The FileProvider stays private, read-only by grant, and limited to the export folder and held items. */
    @Test
    fun file_provider_is_not_exported_and_reaches_only_items_and_the_export() {
        val manifest = File(main, "AndroidManifest.xml").readText()
        val provider = Regex("""<provider[^>]*ItemFileProvider[\s\S]*?</provider>""").find(manifest)?.value ?: error("no provider")
        assertTrue(provider.contains("""android:exported="false""""))
        assertTrue(provider.contains("""android:grantUriPermissions="true""""))
        val paths = File(main, "res/xml/file_paths.xml").readText()
        val entries = Regex("""<([a-z-]+-path)\s+name="([^"]+)"\s+path="([^"]*)"""").findAll(paths).map { it.groupValues.drop(1) }.toList()
        assertTrue("paths: $entries", entries == listOf(listOf("cache-path", "export", "export/"), listOf("files-path", "items", "storage/items/")))
        // No write grant, no persistable grant anywhere in the app.
        val kt = File(main, "java/app/localtugboat").walk().filter { it.extension == "kt" }.joinToString("\n") { it.readText() }
        assertFalse(kt.contains("FLAG_GRANT_WRITE_URI_PERMISSION"))
        assertFalse(kt.contains("FLAG_GRANT_PERSISTABLE_URI_PERMISSION"))
        assertFalse(kt.contains("FLAG_GRANT_PREFIX_URI_PERMISSION"))
    }
}
