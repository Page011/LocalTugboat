package app.localtugboat

import java.io.File
import java.nio.file.Files
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Remote unpair, the CameraX temp sweep and the SnapViewModel field order.
 *
 * Nothing in the unit tests constructs SnapViewModel (it needs an Application and the native core), so a field used by
 * init but declared below it crashed the app at launch with every test green. [declaration_order] reads the source and
 * fails for that mistake.
 */
class InitOrderTest {
    // ---- a remote unpair must not leave the success screen up ----

    @Test fun the_paired_with_screen_is_dropped_when_the_pairing_goes() {
        assertEquals(PairUi.Idle, UiLogic.pairingAfterUnpair(PairUi.Done("Office PC")))
    }

    @Test fun a_pairing_in_progress_or_a_failure_is_left_alone() {
        val code = PairUi.Code("482 913", "Office PC")
        assertEquals(code, UiLogic.pairingAfterUnpair(code))
        assertEquals(PairUi.Dialing, UiLogic.pairingAfterUnpair(PairUi.Dialing))
        val failed = PairUi.Failed("CODE EXPIRED")
        assertEquals(failed, UiLogic.pairingAfterUnpair(failed))
        assertEquals(PairUi.Idle, UiLogic.pairingAfterUnpair(PairUi.Idle))
    }

    @Test fun after_a_remote_unpair_onboarding_starts_at_the_scanner_not_the_done_screen() {
        val rows = UiLogic.permRows(PermState(true, false, LocationAccess.NONE, true, true, batteryExempt = false), emptySet(), 34)
        val before = UiLogic.onboardingStep(rows, true, PairUi.Done("Office PC"))
        assertEquals(OnboardingStep.DONE, before) // the bug: this is what the phone showed
        assertEquals(OnboardingStep.SCAN, UiLogic.onboardingStep(rows, true, UiLogic.pairingAfterUnpair(PairUi.Done("Office PC"))))
    }

    @Test fun the_unpaired_event_resets_the_view_model() {
        val vm = source("SnapViewModel.kt")
        assertTrue(Regex("""is FfiEvent\.Unpaired\s*->\s*\{\s*resetAfterUnpair\(\)""").containsMatchIn(vm))
        // the user's own unpair goes through the same reset
        assertTrue(vm.substringAfter("fun unpair()").substringBefore("fun resetAfterUnpair").contains("resetAfterUnpair()"))
    }

    // ---- CameraX temp files left by a killed process ----

    @Test fun abandoned_camerax_temp_files_are_deleted_and_nothing_else() {
        val dir = Files.createTempDirectory("shots").toFile()
        try {
            val now = 10_000_000L
            fun f(name: String, ageMs: Long) = File(dir, name).apply { writeText("x"); setLastModified(now - ageMs) }
            val old = f("CameraX123.tmp", 120_000)
            val fresh = f("CameraX456.tmp", 5_000) // the camera may be writing it right now
            val shot = f("IMG_1.jpg", 120_000)
            val video = f("VID_1.mp4", 120_000)
            assertEquals(1, Capture.sweepCameraTemps(dir, now, 60_000))
            assertFalse(old.exists())
            assertTrue(fresh.exists())
            assertTrue(shot.exists()) // a shot is recover()'s to hand over, never swept
            assertTrue(video.exists())
            // the next sweep, a minute later, takes the one that was fresh
            assertEquals(1, Capture.sweepCameraTemps(dir, now + 120_000, 60_000))
            assertFalse(fresh.exists())
        } finally {
            dir.deleteRecursively()
        }
    }

    @Test fun recover_sweeps_camera_temps() {
        assertTrue(source("Import.kt").substringAfter("fun recover(").contains("sweepCameraTemps(shotsDir(c), nowMs, minAgeMs)"))
    }

    // ---- no field is read before it is assigned ----

    @Test fun declaration_order_guard_finds_the_launch_crash() {
        // the shape of the real bug: init -> refresh() -> a dispatcher declared further down
        val bad = """
            class X {
                private val a = 1
                init {
                    refresh()
                }
                fun refresh() { use(dispatcher) }
                private val dispatcher = Foo()
            }
        """.trimIndent()
        assertEquals(listOf("init uses dispatcher, declared below it"), InitOrder.violations(bad))
        val ok = """
            class X {
                private val a = 1
                private val dispatcher = Foo()
                init {
                    refresh()
                }
                fun refresh() { use(dispatcher) }
            }
        """.trimIndent()
        assertEquals(emptyList<String>(), InitOrder.violations(ok))
    }

    @Test fun declaration_order() {
        val bad = InitOrder.violations(classBody(source("SnapViewModel.kt"), "SnapViewModel"))
        assertEquals("fields read before they are assigned: $bad", emptyList<String>(), bad)
    }

    private val main: File = listOf(File("src/main"), File("app/src/main")).first { it.isDirectory }
    private fun source(name: String) = File(File(main, "java/app/localtugboat"), name).readText()

    /** The text between the class line's brace and the end of the file: members are at 4 spaces of indent. */
    private fun classBody(src: String, name: String): String = src.substringAfter("class $name(").substringAfter("{\n").let { it }

    /**
     * Kotlin runs property initializers and init blocks top to bottom. A member read (directly, or through a function
     * it calls) before its declaration has run is null. Members are the lines at exactly 4 spaces of indent.
     */
    object InitOrder {
        private class Member(val text: String, val prop: String?, val fn: String?, val init: Boolean)

        private val propRe = Regex("""^\s*(?:(?:private|internal|protected|public|override|@\w+(?:\([^)]*\))?)\s+)*(?:val|var)\s+(\w+)""")
        private val fnRe = Regex("""\bfun\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?(\w+)\s*\(""")

        /** A bare name: not after a dot (it.paired is somebody else's field) or inside a longer word. */
        private val ident = Regex("""(?<![.\w])[A-Za-z_]\w*""")

        private fun strip(s: String) = s.replace(Regex("""/\*[\s\S]*?\*/"""), "").replace(Regex("""//[^\n]*"""), "")

        private fun members(body: String): List<Member> {
            val out = mutableListOf<StringBuilder>()
            for (line in strip(body).lines()) {
                val top = line.startsWith("    ") && !line.startsWith("     ") && line.isNotBlank() &&
                    !line.trimStart().startsWith("}") && !line.trimStart().startsWith(")") && !line.trimStart().startsWith(".")
                if (top || out.isEmpty()) out += StringBuilder()
                out.last().appendLine(line)
            }
            return out.map { sb ->
                val t = sb.toString()
                val first = t.lines().first { it.isNotBlank() }
                val prop = propRe.find(first)?.groupValues?.get(1)
                // a getter or a lazy delegate has no value to read early; "get()" on the first line only
                val backed = prop != null && !Regex("""\bget\(\)""").containsMatchIn(first.substringBefore("=")) && !first.contains(" by lazy")
                Member(t, if (backed) prop else null, fnRe.find(first)?.groupValues?.get(1), first.trim().startsWith("init"))
            }
        }

        /** One message per member that reads a later field. Empty = fine. */
        fun violations(body: String): List<String> {
            val ms = members(body)
            val fns = ms.filter { it.fn != null }.associateBy { it.fn!! }
            val problems = mutableListOf<String>()
            for ((i, m) in ms.withIndex()) {
                if (!(m.init || m.prop != null)) continue
                // text this member runs while the class is built: itself and every member function it can reach
                val seen = mutableSetOf<String>()
                val queue = ArrayDeque<String>()
                var text = m.text
                fun scan(t: String) {
                    for (w in ident.findAll(t)) {
                        val n = w.value
                        if (n in fns && seen.add(n)) { queue.add(n); text += "\n" + fns[n]!!.text }
                    }
                }
                scan(m.text)
                while (queue.isNotEmpty()) scan(fns[queue.removeFirst()]!!.text)
                val words = ident.findAll(text).map { it.value }.toSet()
                for ((j, later) in ms.withIndex()) {
                    if (j <= i || later.prop == null) continue
                    if (later.prop in words) problems += "${m.prop ?: "init"} uses ${later.prop}, declared below it"
                }
            }
            return problems
        }
    }
}
