package app.localtugboat

import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class PagedModelTest {
    /** A fake core: 0 until [total], cursor = index of the next row. Records every limit asked for. */
    private class Fake(val total: Int) {
        val limits = mutableListOf<Int>()
        var fail: String? = null
        val calls get() = limits.size

        suspend fun load(cursor: String?, limit: Int): PageOf<Int> {
            limits += limit
            fail?.let { throw IllegalStateException(it) }
            val from = cursor?.toInt() ?: 0
            val to = minOf(from + limit, total)
            return PageOf((from until to).toList(), if (to < total) to.toString() else null)
        }
    }

    private fun model(f: Fake) = PagedModel<Int>(60) { c, n -> f.load(c, n) }

    @Test fun reload_loads_one_page_and_knows_there_is_more() = runBlocking {
        val m = model(Fake(150))
        assertFalse(m.loaded)
        m.reload()
        assertTrue(m.loaded)
        assertEquals(60, m.rows.size)
        assertTrue(m.hasMore)
        assertNull(m.error)
    }

    @Test fun load_more_appends_until_the_end_then_stops_calling_the_core() = runBlocking {
        val f = Fake(150)
        val m = model(f)
        m.reload()
        m.loadMore()
        m.loadMore()
        assertEquals((0 until 150).toList(), m.rows)
        assertFalse(m.hasMore)
        val calls = f.calls
        m.loadMore()
        assertEquals(calls, f.calls)
    }

    @Test fun reload_keeps_as_many_rows_as_are_on_screen() = runBlocking {
        val f = Fake(300)
        val m = model(f)
        m.reload()
        m.loadMore()
        m.loadMore() // 180 rows
        m.reload()
        assertEquals(180, f.limits.last())
        assertEquals(180, m.rows.size)
        assertTrue(m.hasMore)
        m.loadMore() // continues after the reloaded block, no gap and no repeat
        assertEquals((0 until 240).toList(), m.rows)
    }

    @Test fun reload_never_asks_for_more_than_the_core_allows_and_keeps_every_row_on_screen() = runBlocking {
        val f = Fake(700)
        val m = model(f)
        m.reload()
        repeat(8) { m.loadMore() } // 540 rows
        assertEquals(540, m.rows.size)
        f.limits.clear()
        m.reload()
        assertTrue("every limit within the core cap: ${f.limits}", f.limits.all { it <= 500 })
        assertEquals((0 until 540).toList(), m.rows)
        assertTrue(m.hasMore)
        m.loadMore() // continues after the reloaded block
        assertEquals((0 until 600).toList(), m.rows)
    }

    @Test fun a_reload_that_fails_part_way_keeps_the_old_rows() = runBlocking {
        val f = Fake(700)
        var calls = 0
        val m = PagedModel<Int>(60) { c, n ->
            calls++
            if (calls > 10) throw IllegalStateException("cut off") // calls 1-9 build 540 rows, 10 = first page of the reload, 11 = its second page
            f.load(c, n)
        }
        m.reload()
        repeat(8) { m.loadMore() } // 540 rows, 9 calls
        m.reload() // the first page of 500 comes back, the second page fails: nothing is half-applied
        assertEquals(11, calls)
        assertEquals((0 until 540).toList(), m.rows)
        assertEquals("cut off", m.error)
    }

    @Test fun a_failed_reload_keeps_the_rows_and_says_why_and_the_next_success_clears_it() = runBlocking {
        val f = Fake(100)
        val m = model(f)
        m.reload()
        f.fail = "database busy"
        m.reload()
        assertEquals(60, m.rows.size)
        assertEquals("database busy", m.error)
        f.fail = null
        m.reload()
        assertNull(m.error)
    }

    @Test fun a_first_load_that_fails_is_loaded_with_an_error_and_no_rows() = runBlocking {
        val f = Fake(100).apply { fail = "no core" }
        val m = model(f)
        m.reload()
        assertTrue(m.loaded)
        assertTrue(m.rows.isEmpty())
        assertEquals("no core", m.error)
    }

    @Test fun a_failed_load_more_can_be_retried_from_the_same_place() = runBlocking {
        val f = Fake(150)
        val m = model(f)
        m.reload()
        f.fail = "timeout"
        m.loadMore()
        assertEquals(60, m.rows.size)
        assertNotNull(m.error)
        assertTrue(m.hasMore)
        f.fail = null
        m.loadMore()
        assertEquals(120, m.rows.size)
        assertEquals((0 until 120).toList(), m.rows)
    }

    @Test fun calls_never_overlap_so_rows_cannot_interleave() = runBlocking {
        val inFlight = AtomicInteger()
        val worst = AtomicInteger()
        val m = PagedModel<Int>(10) { c, n ->
            val now = inFlight.incrementAndGet()
            worst.updateAndGet { maxOf(it, now) }
            delay(20)
            inFlight.decrementAndGet()
            val from = c?.toInt() ?: 0
            PageOf((from until from + n).toList(), (from + n).toString())
        }
        m.reload()
        listOf(async { m.loadMore() }, async { m.reload() }, async { m.loadMore() }).awaitAll()
        assertEquals(1, worst.get())
        assertEquals(m.rows, m.rows.indices.toList()) // contiguous 0..n-1
    }

    @Test fun nav_stack_has_the_camera_at_the_bottom_and_back_leaves_it() {
        val n = NavStack()
        assertEquals(Route.Camera, n.top)
        assertFalse(n.back())
        n.go(Route.Gallery(null))
        n.go(Route.Gallery(null)) // same screen twice does not stack
        n.go(Route.Viewer(null, false, "i1"))
        assertEquals(3, n.routes.size)
        n.replaceTop(Route.Viewer(null, false, "i2"))
        assertEquals(Route.Viewer(null, false, "i2"), n.top)
        assertTrue(n.back())
        assertEquals(Route.Gallery(null), n.top)
        n.replaceTop(Route.Gallery("f1")) // folder tab: same level, new filter
        assertEquals(2, n.routes.size)
        assertEquals(Route.Gallery("f1"), n.top)
        n.home()
        assertEquals(listOf<Route>(Route.Camera), n.routes.toList())
    }

    @Test fun replace_top_on_the_camera_adds_instead_of_overwriting_it() {
        val n = NavStack()
        n.replaceTop(Route.Settings)
        assertEquals(listOf(Route.Camera, Route.Settings), n.routes.toList())
    }
}
