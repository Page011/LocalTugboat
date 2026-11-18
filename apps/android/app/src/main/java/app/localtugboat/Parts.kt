package app.localtugboat

import androidx.activity.compose.LocalActivity
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.unit.dp
import androidx.core.view.WindowCompat

/**
 * Light system bar icons while a full-bleed dark screen (camera, viewer) is shown, so the clock and battery
 * stay readable over the preview in the light theme. Restores the previous look when the screen goes away.
 */
@Composable
fun LightSystemIcons() {
    val view = LocalView.current
    val window = LocalActivity.current?.window ?: return
    DisposableEffect(window) {
        val c = WindowCompat.getInsetsController(window, view)
        val prevStatus = c.isAppearanceLightStatusBars
        val prevNav = c.isAppearanceLightNavigationBars
        c.isAppearanceLightStatusBars = false
        c.isAppearanceLightNavigationBars = false
        onDispose {
            c.isAppearanceLightStatusBars = prevStatus
            c.isAppearanceLightNavigationBars = prevNav
        }
    }
}

/** One permission: what it is for and its state. The state is words, not colour alone. */
@Composable
fun PermissionRow(row: PermRow, onAllow: () -> Unit, onSettings: () -> Unit, modifier: Modifier = Modifier) {
    val cs = MaterialTheme.colorScheme
    val state = when (row.status) {
        PermStatus.GRANTED -> "allowed"
        PermStatus.NEEDED -> "needed"
        PermStatus.BLOCKED -> "blocked"
    }
    Row(modifier.fillMaxWidth().defaultMinSize(minHeight = 64.dp).padding(vertical = 6.dp).described("${row.title}, $state"), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(row.title, style = MaterialTheme.typography.bodyLarge)
            Text(row.why, style = MaterialTheme.typography.bodySmall, color = cs.onSurfaceVariant)
            if (row.required) Text("Required", style = MaterialTheme.typography.labelSmall, color = cs.onSurfaceVariant)
        }
        Spacer(Modifier.width(12.dp))
        when (row.status) {
            PermStatus.GRANTED -> Row(verticalAlignment = Alignment.CenterVertically) {
                Ico(R.drawable.ic_check, null, tint = cs.primary, size = 20)
                Spacer(Modifier.width(4.dp))
                Text("Allowed", style = MaterialTheme.typography.labelLarge, color = cs.primary)
            }
            PermStatus.NEEDED -> FilledTonalButton(onClick = onAllow, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Allow") }
            PermStatus.BLOCKED -> Column(horizontalAlignment = Alignment.End, verticalArrangement = Arrangement.Center) {
                Text("BLOCKED", style = MaterialTheme.typography.labelMedium, color = cs.error)
                TextButton(onClick = onSettings, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Open settings") }
            }
        }
    }
}

/** Section title inside a scrolling settings-like page. */
@Composable
fun SectionTitle(text: String, modifier: Modifier = Modifier) {
    Text(text, style = MaterialTheme.typography.titleSmall, color = MaterialTheme.colorScheme.primary, modifier = modifier.fillMaxWidth().padding(top = 20.dp, bottom = 4.dp))
}
