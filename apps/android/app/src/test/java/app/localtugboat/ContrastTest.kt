package app.localtugboat

import androidx.compose.ui.graphics.toArgb
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Measures the colour tokens in Theme.kt with the WCAG formula. The table in Theme.kt is the output of this
 * test's arithmetic; if a token is edited and a text pair drops under 4.5:1 (or an outline under 3:1) this fails.
 */
class ContrastTest {
    private fun channel(v: Int): Double {
        val c = v / 255.0
        return if (c <= 0.03928) c / 12.92 else Math.pow((c + 0.055) / 1.055, 2.4)
    }

    private fun lum(rgb: Int) = 0.2126 * channel(rgb shr 16 and 255) + 0.7152 * channel(rgb shr 8 and 255) + 0.0722 * channel(rgb and 255)

    private fun ratio(a: Int, b: Int): Double {
        val (hi, lo) = lum(a).let { x -> lum(b).let { y -> maxOf(x, y) to minOf(x, y) } }
        return (hi + 0.05) / (lo + 0.05)
    }

    private class Pair(val name: String, val fg: (Palette) -> Int, val bg: (Palette) -> Int, val min: Double)

    private val pairs = listOf(
        Pair("onSurface/surface", { it.onSurface }, { it.surface }, 4.5),
        Pair("onSurface/surfaceContainer", { it.onSurface }, { it.surfaceContainer }, 4.5),
        Pair("onSurface/surfaceContainerHigh", { it.onSurface }, { it.surfaceContainerHigh }, 4.5),
        Pair("onSurfaceVariant/surface", { it.onSurfaceVariant }, { it.surface }, 4.5),
        Pair("onSurfaceVariant/surfaceVariant", { it.onSurfaceVariant }, { it.surfaceVariant }, 4.5),
        Pair("onSurfaceVariant/surfaceContainer", { it.onSurfaceVariant }, { it.surfaceContainer }, 4.5),
        Pair("onPrimary/primary", { it.onPrimary }, { it.primary }, 4.5),
        Pair("onPrimaryContainer/primaryContainer", { it.onPrimaryContainer }, { it.primaryContainer }, 4.5),
        Pair("onSecondary/secondary", { it.onSecondary }, { it.secondary }, 4.5),
        Pair("onSecondaryContainer/secondaryContainer", { it.onSecondaryContainer }, { it.secondaryContainer }, 4.5),
        Pair("onTertiary/tertiary", { it.onTertiary }, { it.tertiary }, 4.5),
        Pair("onTertiaryContainer/tertiaryContainer", { it.onTertiaryContainer }, { it.tertiaryContainer }, 4.5),
        Pair("onError/error", { it.onError }, { it.error }, 4.5),
        Pair("onErrorContainer/errorContainer", { it.onErrorContainer }, { it.errorContainer }, 4.5),
        Pair("onAction/action", { it.onAction }, { it.action }, 4.5),
        Pair("primary text on surface", { it.primary }, { it.surface }, 4.5),
        Pair("error text on surface", { it.error }, { it.surface }, 4.5),
        Pair("tertiary text on surface", { it.tertiary }, { it.surface }, 4.5),
        Pair("primary text on surfaceContainer", { it.primary }, { it.surfaceContainer }, 4.5),
        Pair("onSurface/surfaceVariant", { it.onSurface }, { it.surfaceVariant }, 4.5),
        Pair("outline on surface", { it.outline }, { it.surface }, 3.0),
        Pair("outline on surfaceContainer", { it.outline }, { it.surfaceContainer }, 3.0),
        Pair("action edge on surface", { it.action }, { it.surface }, 3.0),
        Pair("ok text on surface", { it.ok }, { it.surface }, 4.5),
        Pair("ok text on surfaceContainer", { it.ok }, { it.surfaceContainer }, 4.5),
        Pair("outline on surfaceContainerHigh", { it.outline }, { it.surfaceContainerHigh }, 3.0),
    )

    @Test fun every_text_pair_is_4_5_and_outline_is_3_in_light_and_dark() {
        val fails = buildList {
            for ((label, p) in listOf("light" to Tokens.light, "dark" to Tokens.dark)) {
                for (pair in pairs) {
                    val r = ratio(pair.fg(p), pair.bg(p))
                    if (r < pair.min) add("$label ${pair.name} %.2f < ${pair.min}".format(r))
                }
            }
        }
        assertTrue(fails.joinToString("\n"), fails.isEmpty())
    }

    // Text over the live preview sits on a 70% black scrim (alpha 0xB3). Worst case is a pure white scene: white
    // under 70% black = 255 * (1 - 0xB3 / 255) = #4C4C4C.
    private val worstScrimBackground = (255 - 0xB3).let { (it shl 16) or (it shl 8) or it }

    @Test fun white_text_on_the_scrim_over_a_white_scene_is_at_least_4_5() {
        val r = ratio(0xFFFFFF, worstScrimBackground)
        assertTrue("white on scrim %.2f".format(r), r >= 4.5)
    }

    @Test fun camera_status_dots_are_visible_on_the_worst_scrim() {
        for (dot in listOf(ScrimDots.OK, ScrimDots.BUSY, ScrimDots.WARN, ScrimDots.BAD)) {
            val r = ratio(dot, worstScrimBackground)
            assertTrue("dot %06X on scrim %.2f".format(dot, r), r >= 3.0)
        }
    }

    // The shutter sits on a 75% black disc (ShutterDisc, alpha 0xBF): over a pure white scene that is
    // 255 * (1 - 0xBF / 255) = #404040. The orange ring (ShutterRing #FF8E45) must hold 3:1 there.
    @Test fun shutter_ring_is_at_least_3_on_the_worst_disc() {
        val r = ratio(0xFF8E45, 0x404040)
        assertTrue("ring on disc %.2f".format(r), r >= 3.0)
        // The constants the test reasons about are the ones the camera screen draws.
        assertTrue((ShutterRing.toArgb() and 0xFFFFFF) == 0xFF8E45)
        assertTrue((ShutterDisc.toArgb() ushr 24) == 0xBF)
    }

    @Test fun scrim_constant_is_the_70_percent_the_worst_case_assumes() {
        assertTrue((Scrim.toArgb() ushr 24) == 0xB3)
    }

    // Material draws an unselected FilterChip border (and dividers) in outlineVariant: it is a control edge, so
    // it must hold 3:1 on the surface it sits on, in both themes (it was the pale surfaceVariant, 1.1 / 1.5:1).
    @Test fun scheme_outline_variant_is_a_3_to_1_control_edge() {
        for ((label, p, scheme) in listOf(Triple("light", Tokens.light, Tokens.light.light()), Triple("dark", Tokens.dark, Tokens.dark.dark()))) {
            val edge = scheme.outlineVariant.toArgb() and 0xFFFFFF
            for (bg in listOf(p.surface, p.surfaceContainer)) {
                val r = ratio(edge, bg)
                assertTrue("$label outlineVariant on %06X %.2f".format(bg, r), r >= 3.0)
            }
        }
    }

    @Test fun error_red_is_clearly_red_not_plum() {
        // A blocker must read as red (rule: urgent is always red): the red channel leads by 60 in both themes,
        // and the pink the dark theme used to have (green and blue both over 0xA0) is out.
        for (e in listOf(Tokens.light.error, Tokens.dark.error, ScrimDots.BAD)) {
            val r = e shr 16 and 255
            val g = e shr 8 and 255
            val b = e and 255
            assertTrue("error %06X".format(e), r >= g + 60 && r >= b + 60 && g < 0xA0 && b < 0xA0)
        }
    }
}
