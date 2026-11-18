package app.localtugboat

import android.content.ActivityNotFoundException
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    private val vm: SnapViewModel by viewModels()

    // After a dialog: a denied permission with no rationale left will never show a dialog again, so its
    // Allow button must open app settings (see UiLogic.hardDenied).
    private val askPermissions = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { results ->
        val kinds = results.keys.mapNotNull(UiLogic::permKind).toSet()
        val noRationale = results.filter { !it.value && !shouldShowRequestPermissionRationale(it.key) }.keys.mapNotNull(UiLogic::permKind).toSet()
        vm.onPermissionResult(kinds, noRationale)
    }

    // The battery optimisation list returns a result when the user leaves it; the vendor screen follows.
    private val exemption = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { openVendorScreen() }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val actions = ShellActions(
            request = { kind ->
                // Below Android 13 notifications have no dialog: only the switch in settings.
                if (kind == PermKind.NOTIFICATIONS && Build.VERSION.SDK_INT < 33) startActivity(Perms.appSettingsIntent(this))
                else askPermissions.launch(UiLogic.permissionsFor(kind))
            },
            openAppSettings = { startActivity(Perms.appSettingsIntent(this)) },
            exportLog = { lifecycleScope.launch { vm.exportLog()?.let { startActivity(it) } } },
            fixBattery = {
                vm.acknowledgeGap()
                val step1 = BatteryFix.exemptionIntent(this)
                if (step1 == null) {
                    openVendorScreen()
                } else {
                    try {
                        exemption.launch(step1)
                    } catch (e: ActivityNotFoundException) {
                        // A phone without that list: go straight to the vendor screen (and say so in the log).
                        KLog.log(this, "battery fix: no optimisation list on this phone")
                        openVendorScreen()
                    }
                }
            },
        )
        setContent { SnapTheme { Shell(vm, actions) } }
        // Not after a configuration change or process restore: the same intent would run again.
        if (savedInstanceState == null) handleIntent(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleIntent(intent)
    }

    override fun onStart() {
        super.onStart()
        vm.onForeground()
    }

    override fun onResume() {
        super.onResume()
        vm.onPermissionsChanged() // back from a system settings screen
    }

    override fun onStop() {
        vm.onBackground()
        super.onStop()
    }

    private fun openVendorScreen() {
        try {
            KLog.log(this, "battery fix: opened ${BatteryFix.openVendorScreen(this)}")
        } catch (e: Exception) {
            core.reportError("battery fix: ${e.message}")
        }
    }

    /**
     * A pairing link only ever shows the "Pair with this PC?" dialog. Intent extras are never read:
     * any app can send an intent, so nothing in it may decide what we connect to.
     */
    private fun handleIntent(intent: Intent) {
        if (intent.action == Intent.ACTION_VIEW) vm.onDeepLink(intent.dataString)
    }
}
