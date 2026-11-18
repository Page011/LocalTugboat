package app.localtugboat

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/** One page of rows and the cursor for the next page (null = this was the last one). */
class PageOf<T>(val rows: List<T>, val next: String?)

/**
 * Rows of a list screen (gallery, Bin, activity), loaded page by page. The loader is a plain function so
 * the rules here are tested without the core.
 *
 * - [reload] re-reads from the top, as many rows as are on screen (at least one page), so a change event
 *   never scrolls the user back or drops rows they already scrolled to.
 * - A failed load keeps the rows already shown and sets [error]; the next success clears it.
 * - Calls run one at a time: a reload during a loadMore waits, so rows never interleave.
 */
class PagedModel<T>(private val pageSize: Int = 60, private val load: suspend (cursor: String?, limit: Int) -> PageOf<T>) {
    var rows by mutableStateOf<List<T>>(emptyList())
        private set
    var loaded by mutableStateOf(false)
        private set
    var error by mutableStateOf<String?>(null)
        private set
    var hasMore by mutableStateOf(false)
        private set

    private var next: String? = null
    private val lock = Mutex()

    suspend fun reload() = lock.withLock {
        guarded {
            // As many rows as are on screen, in pages the core accepts. Nothing is committed until every page came back.
            val target = rows.size.coerceAtLeast(pageSize)
            val acc = ArrayList<T>(target)
            var cursor: String? = null
            var more: String?
            do {
                val p = load(cursor, minOf(target - acc.size, MAX_LIMIT).coerceAtLeast(1))
                acc += p.rows
                more = p.next
                cursor = more
            } while (acc.size < target && more != null)
            rows = acc
            next = more
            hasMore = more != null
        }
    }

    suspend fun loadMore() = lock.withLock {
        val cursor = next ?: return@withLock
        guarded {
            val p = load(cursor, pageSize)
            rows = rows + p.rows
            next = p.next
            hasMore = p.next != null
        }
    }

    private suspend fun guarded(block: suspend () -> Unit) {
        try {
            block()
            error = null
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            error = e.message ?: e.javaClass.simpleName
        } finally {
            loaded = true
        }
    }

    private companion object {
        /** The core's own cap for one page. */
        const val MAX_LIMIT = 500
    }
}

/** Where the user is inside the paired app. The camera is the root and cannot be popped. */
class NavStack {
    val routes = mutableStateListOf<Route>(Route.Camera)

    val top: Route get() = routes.last()

    fun go(r: Route) {
        if (top != r) routes += r
    }

    /** False when already at the camera: the system back button then leaves the app. */
    fun back(): Boolean {
        if (routes.size <= 1) return false
        routes.removeAt(routes.lastIndex)
        return true
    }

    fun replaceTop(r: Route) {
        if (routes.size > 1) routes[routes.lastIndex] = r else routes += r
    }

    fun home() {
        routes.clear()
        routes += Route.Camera
    }
}
