package app.localtugboat

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.itemsIndexed
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.FilterChip
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.filter
import kotlinx.coroutines.launch
import uniffi.tugboat_ffi.FfiEvent
import uniffi.tugboat_ffi.FolderInfo
import uniffi.tugboat_ffi.ItemInfo
import androidx.compose.ui.text.style.TextOverflow

/**
 * Loads [model] when the screen opens and again after a burst of core events [relevant] to it. The pause
 * collapses a sync that fires hundreds of events into one reload.
 */
@Composable
fun ReloadOnEvents(vm: SnapViewModel, model: PagedModel<*>, relevant: (FfiEvent) -> Boolean) {
    LaunchedEffect(model) { model.reload() }
    LaunchedEffect(model) {
        // Filter BEFORE collectLatest: it cancels the running block on every new value, so an unrelated event (Sync
        // status arrives right after each ItemsChanged) would cancel the pause and the reload would never run.
        vm.events.filter(relevant).collectLatest {
            delay(150)
            model.reload()
        }
    }
}

fun itemsRelevant(e: FfiEvent) = e is FfiEvent.ItemsChanged || e == FfiEvent.Resync || e is FfiEvent.FoldersChanged

/** Loading, error, empty and filtered-empty states of a list, one place for all list screens. */
@Composable
fun ListStates(
    model: PagedModel<*>, folderName: String?, bin: Boolean, onClearFilter: (() -> Unit)?, onRetry: () -> Unit,
    emptyText: String? = null, content: @Composable () -> Unit,
) {
    when (val s = UiLogic.listUi(model.rows.size, model.loaded, model.error, folderName, bin)) {
        UiLogic.ListUi.Content -> content()
        UiLogic.ListUi.Loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
        is UiLogic.ListUi.Failed -> Notice("CANNOT LOAD - ${s.message}", Modifier.fillMaxSize(), "Try again", onRetry, blocker = true)
        is UiLogic.ListUi.Empty -> (emptyText ?: s.text).let { text ->
            Notice(text, Modifier.fillMaxSize(), if (onClearFilter != null) "Show all folders" else null, { onClearFilter?.invoke() }, art = text == UiLogic.NOTHING_YET)
        }
    }
}

@Composable
fun GalleryScreen(vm: SnapViewModel, ui: UiState, banners: List<UiLogic.BannerUi>, onBanner: (UiLogic.BannerUi) -> Unit, route: Route.Gallery) {
    val folders by vm.folders.collectAsStateWithLifecycle()
    val model = vm.itemsModel(route.folderId, false)
    ReloadOnEvents(vm, model, ::itemsRelevant)
    val nowMs by rememberNow(30_000)
    val gridState = rememberLazyGridState()
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    val folderName = route.folderId?.let { id -> folders.firstOrNull { it.id == id }?.name ?: "this folder" }
    val binDays = ui.settings?.binDays ?: 7u
    val all = UiLogic.syncBanner(ui.status)?.let { banners + it } ?: banners

    // Next page when the last visible cell is near the end of what is loaded.
    LaunchedEffect(model, gridState) {
        snapshotFlow { (gridState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0) to model.rows.size }.collect { (last, size) ->
            if (model.hasMore && last >= size - 12) model.loadMore()
        }
    }

    Column(Modifier.fillMaxSize()) {
        ScreenBar("Gallery", onBack = { vm.nav.back() }) {
            IconBtn(R.drawable.ic_history, "Activity", { vm.nav.go(Route.Activity) })
            IconBtn(R.drawable.ic_delete, "Bin", { vm.nav.go(Route.Bin) })
            IconBtn(R.drawable.ic_settings, "Settings", { vm.nav.go(Route.Settings) })
        }
        BannerStack(all, onBanner, Modifier.padding(horizontal = 12.dp, vertical = 4.dp))
        FolderTabs(folders, route.folderId) { vm.nav.replaceTop(Route.Gallery(it)) }
        Box(Modifier.weight(1f)) {
            ListStates(model, folderName, false, if (route.folderId != null) ({ vm.nav.replaceTop(Route.Gallery(null)) }) else null, { scope.launch { model.reload() } }) {
                val bottom = with(LocalDensity.current) { WindowInsets.navigationBars.getBottom(this).toDp() }
                LazyVerticalGrid(
                    columns = GridCells.Adaptive(112.dp), state = gridState,
                    contentPadding = PaddingValues(start = 4.dp, end = 4.dp, top = 4.dp, bottom = 4.dp + bottom),
                    horizontalArrangement = Arrangement.spacedBy(4.dp), verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    itemsIndexed(model.rows, key = { _, it -> it.id }) { _, item ->
                        ItemCell(item, UiLogic.cell(item, nowMs, binDays, false)) { vm.nav.go(Route.Viewer(route.folderId, false, item.id)) }
                    }
                }
            }
        }
    }
}

@Composable
private fun FolderTabs(folders: List<FolderInfo>, selected: String?, onPick: (String?) -> Unit) {
    LazyRow(contentPadding = PaddingValues(horizontal = 12.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        item("all") { FolderTab("All", selected == null) { onPick(null) } }
        items(folders, key = { it.id }) { f -> FolderTab("${f.name} ${f.count}", f.id == selected) { onPick(f.id) } }
    }
}

/** A selected tab shows a check mark and a filled tint: not colour alone. */
@Composable
private fun FolderTab(label: String, selected: Boolean, onClick: () -> Unit) {
    FilterChip(
        selected = selected, onClick = onClick, label = { Text(label, maxLines = 1, overflow = TextOverflow.Ellipsis) },
        leadingIcon = if (selected) ({ Ico(R.drawable.ic_check, null, size = 18) }) else null,
    )
}

@Composable
private fun Badge(icon: Int) {
    Box(Modifier.size(24.dp).background(Scrim, CircleShape), contentAlignment = Alignment.Center) { Ico(icon, null, tint = Color.White, size = 16) }
}

/** One thumbnail: badge for video and Keep, label strip underneath. Read by screen readers as one sentence. */
@Composable
private fun ItemCell(item: ItemInfo, cell: UiLogic.CellUi, onClick: () -> Unit) {
    Box(
        Modifier.aspectRatio(1f).clip(RoundedCornerShape(8.dp)).clickable(role = Role.Button, onClick = onClick)
            .described("${UiLogic.kindWord(item.kind)}, ${if (cell.file) FileRules.displayName(item.name, item.ext) + ", " else ""}${cell.label}"),
    ) {
        if (cell.file) FileFace(item, Modifier.fillMaxSize()) else ThumbImage(item.thumbPath, null, Modifier.fillMaxSize())
        Row(Modifier.align(Alignment.TopStart).padding(4.dp), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            if (cell.video) Badge(R.drawable.ic_videocam)
            if (cell.kept) Badge(R.drawable.ic_star_fill1)
        }
        Text(
            cell.label, color = Color.White, style = MaterialTheme.typography.labelSmall, maxLines = 2, overflow = TextOverflow.Ellipsis,
            modifier = Modifier.align(Alignment.BottomStart).fillMaxWidth().background(Scrim).padding(horizontal = 6.dp, vertical = 3.dp),
        )
    }
}

/** Bin: what was deleted or expired, days left, Restore. Tapping a row opens it in the viewer. */
@Composable
fun BinScreen(vm: SnapViewModel, ui: UiState) {
    val model = vm.itemsModel(null, true)
    ReloadOnEvents(vm, model, ::itemsRelevant)
    val nowMs by rememberNow(60_000)
    val binDays = ui.settings?.binDays ?: 7u
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    val listState = rememberLazyListState()

    // Next page when the last visible row is near the end of what is loaded (same rule as the gallery grid).
    LaunchedEffect(model, listState) {
        snapshotFlow { (listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: 0) to model.rows.size }.collect { (last, size) ->
            if (model.hasMore && last >= size - 12) model.loadMore()
        }
    }

    Column(Modifier.fillMaxSize()) {
        ScreenBar("Bin", onBack = { vm.nav.back() })
        Text("Items stay ${binDays} days, then this phone removes its copy.", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp))
        Box(Modifier.weight(1f)) {
            ListStates(model, null, true, null, { scope.launch { model.reload() } }) {
                val bottom = with(LocalDensity.current) { WindowInsets.navigationBars.getBottom(this).toDp() }
                LazyColumn(state = listState, contentPadding = PaddingValues(bottom = bottom)) {
                    items(model.rows, key = { it.id }) { item ->
                        val cell = UiLogic.cell(item, nowMs, binDays, true)
                        Row(
                            Modifier.fillMaxWidth().defaultMinSize(minHeight = 72.dp).clickable(role = Role.Button) { vm.nav.go(Route.Viewer(null, true, item.id)) }.padding(horizontal = 16.dp, vertical = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            if (cell.file) {
                                Box(Modifier.size(56.dp).clip(RoundedCornerShape(8.dp)).background(MaterialTheme.colorScheme.surfaceVariant), contentAlignment = Alignment.Center) {
                                    Ico(fileIcon(item.ext), null, tint = MaterialTheme.colorScheme.onSurfaceVariant, size = 28)
                                }
                            } else {
                                ThumbImage(item.thumbPath, null, Modifier.size(56.dp).clip(RoundedCornerShape(8.dp)))
                            }
                            Spacer(Modifier.width(12.dp))
                            Column(Modifier.weight(1f)) {
                                Text(if (cell.file) FileRules.displayName(item.name, item.ext) else item.name, style = MaterialTheme.typography.bodyLarge, maxLines = 1, overflow = TextOverflow.Ellipsis)
                                Text(cell.label, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                            }
                            TextButton(onClick = { scope.launchRestore(vm, model, item.id) }, modifier = Modifier.defaultMinSize(minHeight = Touch)) {
                                Ico(R.drawable.ic_restore_from_trash, null, size = 20)
                                Spacer(Modifier.width(6.dp))
                                Text("Restore")
                            }
                        }
                    }
                }
            }
        }
    }
}

private fun kotlinx.coroutines.CoroutineScope.launchRestore(vm: SnapViewModel, model: PagedModel<ItemInfo>, id: String) {
    launch {
        vm.restore(id).join()
        model.reload()
    }
}
