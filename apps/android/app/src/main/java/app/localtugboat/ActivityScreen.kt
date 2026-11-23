package app.localtugboat

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import java.time.ZoneId
import kotlinx.coroutines.launch
import uniffi.tugboat_ffi.ActivityInfo
import uniffi.tugboat_ffi.FfiEvent
import androidx.compose.ui.text.style.TextOverflow

/** What happened to the photos and to the sync, newest first. Problems are in words, an icon and the error colour. */
@Composable
fun ActivityScreen(vm: SnapViewModel) {
    val model = remember {
        PagedModel<ActivityInfo>(50) { cursor, n ->
            val rows = vm.activity(cursor?.toLong(), n.toUInt())
            PageOf(rows, if (rows.size >= n) rows.last().id.toString() else null)
        }
    }
    ReloadOnEvents(vm, model) { it == FfiEvent.ActivityAdded || it == FfiEvent.Resync }
    val nowMs by rememberNow(30_000)
    val scope = rememberCoroutineScope()
    val list = rememberLazyListState()
    val zone = ZoneId.systemDefault()

    LaunchedEffect(model, list) {
        snapshotFlow { (list.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0) to model.rows.size }.collect { (last, size) ->
            if (model.hasMore && last >= size - 10) model.loadMore()
        }
    }

    Column(Modifier.fillMaxSize()) {
        ScreenBar("Activity", onBack = { vm.nav.back() })
        Box(Modifier.weight(1f)) {
            ListStates(model, null, false, null, { scope.launch { model.reload() } }, emptyText = "No activity yet.") {
                val bottom = with(LocalDensity.current) { WindowInsets.navigationBars.getBottom(this).toDp() }
                LazyColumn(state = list, contentPadding = PaddingValues(bottom = bottom)) {
                    items(model.rows, key = { it.id }) { a ->
                        val problem = UiLogic.activityIsProblem(a.action)
                        val cs = MaterialTheme.colorScheme
                        Row(Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp).padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
                            if (problem) Ico(R.drawable.ic_sync_problem, null, tint = cs.error) else Spacer(Modifier.width(24.dp))
                            Spacer(Modifier.width(12.dp))
                            Column(Modifier.weight(1f)) {
                                Text(UiLogic.activityText(a.action), style = MaterialTheme.typography.bodyLarge, color = if (problem) cs.error else cs.onSurface)
                                val detail = UiLogic.activityDetail(a.action, a.detail)
                                if (detail.isNotBlank()) Text(detail, style = MaterialTheme.typography.bodySmall, color = cs.onSurfaceVariant, maxLines = 2, overflow = TextOverflow.Ellipsis)
                            }
                            Spacer(Modifier.width(8.dp))
                            Text(UiLogic.activityTime(a.atMs, nowMs, zone), style = MaterialTheme.typography.labelMedium, color = cs.onSurfaceVariant)
                        }
                    }
                }
            }
        }
    }
}
