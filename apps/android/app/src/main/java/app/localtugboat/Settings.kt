package app.localtugboat

import android.os.Build
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle

/** Connection, background behaviour, Bin days, permissions, storage, log export, unpair. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun SettingsScreen(vm: SnapViewModel, ui: UiState, banners: List<UiLogic.BannerUi>, onBanner: (UiLogic.BannerUi) -> Unit, actions: ShellActions) {
    val hardDenied by vm.hardDenied.collectAsStateWithLifecycle()
    val nowMs by rememberNow(5_000)
    var confirmUnpair by rememberSaveable { mutableStateOf(false) }
    val cs = MaterialTheme.colorScheme
    val s = ui.settings
    val perms = ui.perms

    Column(Modifier.fillMaxSize()) {
        ScreenBar("Settings", onBack = { vm.nav.back() })
        Column(Modifier.weight(1f).verticalScroll(rememberScrollState()).navigationBarsPadding().padding(horizontal = 16.dp)) {
            BannerStack(banners, onBanner)

            SectionTitle("Connection")
            Text(ui.paired?.name ?: "Not paired", style = MaterialTheme.typography.titleMedium)
            SyncLine(ui.status, nowMs, Modifier.padding(top = 2.dp))
            Spacer(Modifier.height(8.dp))
            Row(
                Modifier.fillMaxWidth().defaultMinSize(minHeight = 64.dp).semantics(mergeDescendants = true) { stateDescription = if (s?.stayConnected != false) "On" else "Off" },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text("Stay connected in background", style = MaterialTheme.typography.bodyLarge)
                    Text("Off: syncs while the app is open, and every 15 minutes.", style = MaterialTheme.typography.bodySmall, color = cs.onSurfaceVariant)
                }
                Switch(checked = s?.stayConnected ?: true, onCheckedChange = { vm.setStayConnected(it) }, enabled = s != null)
            }
            Row(Modifier.fillMaxWidth().defaultMinSize(minHeight = 64.dp), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Battery", style = MaterialTheme.typography.bodyLarge)
                    if (perms?.batteryExempt == true) {
                        Text("Not limited. Tugboat can restart itself.", style = MaterialTheme.typography.bodySmall, color = cs.onSurfaceVariant)
                    } else {
                        Text("BATTERY LIMITS MAY STOP TUGBOAT", style = MaterialTheme.typography.bodySmall, color = cs.error)
                        Text("Fix: find Tugboat, set Not optimized.", style = MaterialTheme.typography.bodySmall, color = cs.onSurfaceVariant)
                    }
                }
                if (perms?.batteryExempt != true) FilledTonalButton(onClick = actions.fixBattery, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Fix") }
            }

            SectionTitle("Bin")
            Text("Keep deleted items for", style = MaterialTheme.typography.bodyMedium, color = cs.onSurfaceVariant)
            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                UiLogic.binChoices(s?.binDays ?: 7u).forEach { d ->
                    val on = s?.binDays == d
                    FilterChip(
                        selected = on, onClick = { vm.setBinDays(d) }, enabled = s != null, label = { Text(if (d == 1u) "1 day" else "$d days") },
                        leadingIcon = if (on) ({ Ico(R.drawable.ic_check, null, size = 18) }) else null,
                    )
                }
            }

            SectionTitle("Permissions")
            if (perms != null) {
                UiLogic.permRows(perms, hardDenied, Build.VERSION.SDK_INT, withMic = true).forEach { r ->
                    PermissionRow(r, onAllow = { actions.request(r.kind) }, onSettings = actions.openAppSettings)
                }
            }

            SectionTitle("Storage")
            val free = ui.freeBytes
            Text(
                if (free == null) "Free space unknown" else "Free on this phone: ${UiLogic.bytes(free)}",
                style = MaterialTheme.typography.bodyLarge, color = if (ui.storageLow) cs.error else cs.onSurface,
            )
            if (s != null) Text("Downloads from the PC wait below ${UiLogic.bytes(s.storageGuardBytes.toLong())} free.", style = MaterialTheme.typography.bodySmall, color = cs.onSurfaceVariant)

            SectionTitle("More")
            LinkRow("Activity") { vm.nav.go(Route.Activity) }
            LinkRow("Bin") { vm.nav.go(Route.Bin) }
            LinkRow("Export log") { actions.exportLog() }

            SectionTitle("About")
            Text("Local Tugboat ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE})", style = MaterialTheme.typography.bodyLarge)

            Spacer(Modifier.height(20.dp))
            OutlinedButton(onClick = { confirmUnpair = true }, enabled = ui.paired != null, modifier = Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp)) {
                Text("Unpair from PC", color = if (ui.paired != null) cs.error else cs.onSurfaceVariant)
            }
            Spacer(Modifier.height(24.dp))
        }
    }

    if (confirmUnpair) {
        AlertDialog(
            onDismissRequest = { confirmUnpair = false },
            title = { Text("Unpair from ${ui.paired?.name ?: "PC"}?") },
            text = { Text("Photos stay on this phone and on the PC. They stop syncing until you pair again.") },
            confirmButton = {
                TextButton(onClick = {
                    confirmUnpair = false
                    vm.unpair()
                }) { Text("Unpair", color = cs.error) }
            },
            dismissButton = { TextButton(onClick = { confirmUnpair = false }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun LinkRow(label: String, onClick: () -> Unit) {
    Row(Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp).clickable(role = Role.Button, onClick = onClick), verticalAlignment = Alignment.CenterVertically) {
        Text(label, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
        Ico(R.drawable.ic_chevron_right, null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(4.dp))
    }
}
