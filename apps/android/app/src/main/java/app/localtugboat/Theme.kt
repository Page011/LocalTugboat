package app.localtugboat

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.RowScope
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color

/**
 * The whole colour set, as 0xRRGGBB. Plain Ints so ContrastTest can measure every text pair on the JVM;
 * the test fails if a token drifts under 4.5:1 (text) or 3:1 (outline).
 *
 * Ocean palette, in step with the desktop app (apps/desktop/src/app.css): harbour teal = primary, deep-sea
 * navy = secondary and the launcher icon, sea-foam = surfaces, buoy orange = [Palette.action] (the main
 * action: primary buttons and the shutter ring). Amber stays the warning colour, RED stays for blockers.
 *
 * WCAG contrast ratios, measured with the same formula as ContrastTest (text needs 4.5, large text and
 * outlines 3):
 *
 *                              light   dark
 *   onSurface on surface        14.5   15.7
 *   onSurface on container      13.3   13.7   (container = surfaceContainer)
 *   onSurface on containerHigh  12.4   11.6
 *   onSurfaceVariant on surface  6.9    9.5
 *   onSurfaceVariant on surfVar  6.2    6.3
 *   onSurfaceVariant on container 6.3   8.3
 *   onPrimary on primary         6.7    7.9
 *   onPrimaryContainer on it    11.1    8.9
 *   onSecondary on secondary     9.5    8.0
 *   onSecondaryContainer on it  11.6    7.4
 *   onTertiary on tertiary       6.9    7.8   (tertiary = amber, used for warnings)
 *   onTertiaryContainer on it   11.3    7.5
 *   onError on error             6.5    5.6
 *   onErrorContainer on it      10.5    7.1   (blockers)
 *   onAction on action           4.8    7.7   (white / navy-black on buoy orange)
 *   primary / error / tertiary
 *     as text on surface       6.2 / 6.0 / 6.3    8.8 / 6.5 / 10.7   (dark error = clear red, not pink)
 *   ok (synced) on surface       6.4   10.8   (green; on container 5.9 / 9.3)
 *   primary on container         5.6    7.7
 *   outline on surface           4.3    6.5   (edges need 3; on container 4.0 / 5.7)
 *   outlineVariant = outline, so unselected chips keep a 3:1 edge (it was surfaceVariant: 1.1 / 1.5)
 *   action on surface (edge, never text) 4.4  8.0
 *
 * Over the live camera preview text sits on a 70% black scrim ([Scrim]): worst case a pure white scene
 * gives white text on #4C4C4C, 8.6:1; the [ScrimDots] status dots measure 5.0 / 5.1 / 5.1 / 3.1:1 there (red is the lowest). The shutter sits on a darker 75% disc ([ShutterDisc]): worst case
 * #404040, where the orange ring measures 4.4:1.
 */
class Palette(
    val primary: Int, val onPrimary: Int, val primaryContainer: Int, val onPrimaryContainer: Int,
    val secondary: Int, val onSecondary: Int, val secondaryContainer: Int, val onSecondaryContainer: Int,
    val tertiary: Int, val onTertiary: Int, val tertiaryContainer: Int, val onTertiaryContainer: Int,
    val error: Int, val onError: Int, val errorContainer: Int, val onErrorContainer: Int,
    val surface: Int, val onSurface: Int, val surfaceVariant: Int, val onSurfaceVariant: Int,
    val surfaceContainer: Int, val surfaceContainerHigh: Int, val outline: Int,
    /** Buoy orange: the main action. Not part of Material's scheme, read through [LocalPalette]. */
    val action: Int, val onAction: Int,
    /** Synced / success text and dot colour (green, same hue family as the desktop --ok). */
    val ok: Int,
)

object Tokens {
    val light = Palette(
        primary = 0x0A6572, onPrimary = 0xFFFFFF, primaryContainer = 0xD0E9EC, onPrimaryContainer = 0x05303A,
        secondary = 0x1F4A5E, onSecondary = 0xFFFFFF, secondaryContainer = 0xD6E6EC, onSecondaryContainer = 0x0B2A3C,
        tertiary = 0x7A5200, onTertiary = 0xFFFFFF, tertiaryContainer = 0xFFE3AA, onTertiaryContainer = 0x3A2800,
        error = 0xB3261E, onError = 0xFFFFFF, errorContainer = 0xFADAD6, onErrorContainer = 0x5C0F0A,
        surface = 0xEFF6F6, onSurface = 0x0B2533, surfaceVariant = 0xDEEBEC, onSurfaceVariant = 0x3E5865,
        surfaceContainer = 0xE2EDEE, surfaceContainerHigh = 0xD8E6E8, outline = 0x5A7881,
        action = 0xC24E0A, onAction = 0xFFFFFF, ok = 0x17663A,
    )
    val dark = Palette(
        primary = 0x5FC3CF, onPrimary = 0x05242B, primaryContainer = 0x12404C, onPrimaryContainer = 0xD0E9EC,
        secondary = 0x9CC4D6, onSecondary = 0x0B2A3C, secondaryContainer = 0x1D4B5E, onSecondaryContainer = 0xD6E6EC,
        tertiary = 0xF0BF66, onTertiary = 0x422C00, tertiaryContainer = 0x5E4100, onTertiaryContainer = 0xFFE3AA,
        error = 0xFF6B60, onError = 0x4A0A07, errorContainer = 0x8C1D18, onErrorContainer = 0xFFDAD6,
        surface = 0x08171F, onSurface = 0xE4F1F1, surfaceVariant = 0x1F3A47, onSurfaceVariant = 0xA5C0C8,
        surfaceContainer = 0x0F2530, surfaceContainerHigh = 0x17323F, outline = 0x7FA0AE,
        action = 0xFF8E45, onAction = 0x2B1200, ok = 0x84D99E,
    )
}

/** Background of text that floats over the camera preview or a thumbnail: 70% black. */
val Scrim = Color(0xB3000000)

/** Status dot colours over [Scrim]. Measured by ContrastTest against the worst case (a pure white scene, #4C4C4C): all >= 3:1. */
object ScrimDots {
    const val OK = 0x7DD99B
    const val BUSY = 0x7FD4DF
    const val WARN = 0xF0BF66
    const val BAD = 0xFF6B60
}

/** Disc behind the shutter: 75% black, so the orange ring keeps 3:1 even over a white scene. */
val ShutterDisc = Color(0xBF000000)

/** The shutter ring and the buoy-orange accent seen over the camera: bright, because the scene behind is dark. */
val ShutterRing = Color(0xFFFF8E45)

/** An opaque colour from a 0xRRGGBB token. */
internal fun c(v: Int) = Color((0xFF shl 24) or v)

internal fun Palette.light() = lightColorScheme(
    primary = c(primary), onPrimary = c(onPrimary), primaryContainer = c(primaryContainer), onPrimaryContainer = c(onPrimaryContainer),
    secondary = c(secondary), onSecondary = c(onSecondary), secondaryContainer = c(secondaryContainer), onSecondaryContainer = c(onSecondaryContainer),
    tertiary = c(tertiary), onTertiary = c(onTertiary), tertiaryContainer = c(tertiaryContainer), onTertiaryContainer = c(onTertiaryContainer),
    error = c(error), onError = c(onError), errorContainer = c(errorContainer), onErrorContainer = c(onErrorContainer),
    background = c(surface), onBackground = c(onSurface), surface = c(surface), onSurface = c(onSurface),
    surfaceVariant = c(surfaceVariant), onSurfaceVariant = c(onSurfaceVariant), outline = c(outline), outlineVariant = c(outline),
    surfaceContainer = c(surfaceContainer), surfaceContainerHigh = c(surfaceContainerHigh),
    surfaceContainerLow = c(surface), surfaceContainerLowest = c(surface), surfaceContainerHighest = c(surfaceContainerHigh),
)

internal fun Palette.dark() = darkColorScheme(
    primary = c(primary), onPrimary = c(onPrimary), primaryContainer = c(primaryContainer), onPrimaryContainer = c(onPrimaryContainer),
    secondary = c(secondary), onSecondary = c(onSecondary), secondaryContainer = c(secondaryContainer), onSecondaryContainer = c(onSecondaryContainer),
    tertiary = c(tertiary), onTertiary = c(onTertiary), tertiaryContainer = c(tertiaryContainer), onTertiaryContainer = c(onTertiaryContainer),
    error = c(error), onError = c(onError), errorContainer = c(errorContainer), onErrorContainer = c(onErrorContainer),
    background = c(surface), onBackground = c(onSurface), surface = c(surface), onSurface = c(onSurface),
    surfaceVariant = c(surfaceVariant), onSurfaceVariant = c(onSurfaceVariant), outline = c(outline), outlineVariant = c(outline),
    surfaceContainer = c(surfaceContainer), surfaceContainerHigh = c(surfaceContainerHigh),
    surfaceContainerLow = c(surface), surfaceContainerLowest = c(surface), surfaceContainerHighest = c(surfaceContainerHigh),
)

/** The active palette, for the tokens Material's ColorScheme has no slot for (the orange action colour). */
val LocalPalette = staticCompositionLocalOf { Tokens.light }

/** Main action button: buoy orange. Everything else keeps Material's own button colours. */
@Composable
fun ActionButton(onClick: () -> Unit, modifier: Modifier = Modifier, enabled: Boolean = true, content: @Composable RowScope.() -> Unit) {
    val p = LocalPalette.current
    Button(
        onClick = onClick, modifier = modifier, enabled = enabled,
        colors = ButtonDefaults.buttonColors(containerColor = c(p.action), contentColor = c(p.onAction)),
        content = content,
    )
}

/** Light and dark by system setting. Fixed colours: no wallpaper-derived palette, so the measured contrast is the real contrast. */
@Composable
fun SnapTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    val scheme: ColorScheme = if (dark) Tokens.dark.dark() else Tokens.light.light()
    CompositionLocalProvider(LocalPalette provides (if (dark) Tokens.dark else Tokens.light)) {
        MaterialTheme(colorScheme = scheme, content = content)
    }
}
