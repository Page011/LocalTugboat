package app.localtugboat

import android.os.Build
import androidx.compose.foundation.background
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
import androidx.compose.foundation.shape.RoundedCornerShape
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import uniffi.tugboat_ffi.DeviceStatus

/** Linked devices, background behaviour, Bin days, permissions, storage, log export. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
fun SettingsScreen(vm: SnapViewModel, ui: UiState, banners: List<UiLogic.BannerUi>, onBanner: (UiLogic.BannerUi) -> Unit, actions: ShellActions) {
    val hardDenied by vm.hardDenied.collectAsStateWithLifecycle()
    val nowMs by rememberNow(5_000)
    var unlinkId by rememberSaveable { mutableStateOf<String?>(null) }
    val cs = MaterialTheme.colorScheme
    val s = ui.settings
    val perms = ui.perms

    Column(Modifier.fillMaxSize()) {
        ScreenBar("Settings", onBack = { vm.nav.back() })
        Column(Modifier.weight(1f).verticalScroll(rememberScrollState()).navigationBarsPadding().padding(horizontal = 16.dp)) {
            BannerStack(banners, onBanner)

            SectionTitle(UiLogic.linkedHeader(ui.devices))
            ui.devices.forEach { d -> DeviceRow(d, nowMs, onUnlink = { unlinkId = d.id }) }
            val link = UiLogic.linkButton(ui.devices)
            OutlinedButton(onClick = vm::openAddDevice,enabled = link.enabled, modifier = Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp).padding(top = 8.dp)) {
                Text("Link a PC")
            }
            link.limitLine?.let { Text(it, style = MaterialTheme.typography.bodySmall, color = cs.error, modifier = Modifier.padding(top = 4.dp)) }

            SectionTitle("Connection")
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

            Spacer(Modifier.height(24.dp))
        }
    }

    // The row can vanish while the dialog is open (the PC logged out): the dialog goes with it.
    val target = ui.devices.firstOrNull { it.id == unlinkId && !it.unlinking }
    if (target != null) {
        AlertDialog(
            onDismissRequest = { unlinkId = null },
            title = { Text("Unlink ${target.name}?") },
            text = { Text("Files stay on both.") },
            confirmButton = {
                TextButton(onClick = {
                    unlinkId = null
                    vm.unlink(target.id)
                }) { Text("Unlink", color = cs.error) }
            },
            dismissButton = { TextButton(onClick = { unlinkId = null }) { Text("Cancel") } },
        )
    }
}

/** One linked PC: name, one status line (grey when away, red when it needs the user), optional waiting chip, Unlink. */
@Composable
private fun DeviceRow(d: DeviceStatus, nowMs: Long, onUnlink: () -> Unit) {
    val cs = MaterialTheme.colorScheme
    val line = UiLogic.deviceLine(d, nowMs)
    Row(Modifier.fillMaxWidth().defaultMinSize(minHeight = 64.dp), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f).padding(vertical = 6.dp)) {
            Text(d.name, style = MaterialTheme.typography.titleMedium)
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (line.problem) {
                    Chip(line.text, cs.errorContainer, cs.onErrorContainer)
                } else {
                    Text(line.text, style = MaterialTheme.typography.bodyMedium, color = cs.onSurfaceVariant)
                }
                line.waitingChip?.let { Chip(it, cs.surfaceVariant, cs.onSurfaceVariant) }
            }
        }
        if (line.canUnlink) TextButton(onClick = onUnlink, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Unlink", color = cs.error) }
    }
}

/** Filled pill: the whole shape carries the colour, no edge stripe. */
@Composable
private fun Chip(text: String, bg: Color, fg: Color) {
    Text(text, style = MaterialTheme.typography.labelMedium, color = fg, modifier = Modifier.background(bg, RoundedCornerShape(50)).padding(horizontal = 10.dp, vertical = 2.dp))
}

@Composable
private fun LinkRow(label: String, onClick: () -> Unit) {
    Row(Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp).clickable(role = Role.Button, onClick = onClick), verticalAlignment = Alignment.CenterVertically) {
        Text(label, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
        Ico(R.drawable.ic_chevron_right, null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(4.dp))
    }
}
