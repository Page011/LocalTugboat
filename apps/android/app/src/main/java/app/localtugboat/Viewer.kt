package app.localtugboat

import android.net.Uri
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculatePan
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChanged
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.ui.PlayerView
import java.io.File
import java.time.ZoneId
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.tugboat_ffi.ItemInfo
import uniffi.tugboat_ffi.ItemStateInfo
import uniffi.tugboat_ffi.MediaKind
import androidx.compose.ui.text.style.TextOverflow

/**
 * One item full screen, swipe for the next. Photo: pinch, drag, double tap. Video: Media3 with controls.
 * Actions: Keep, Move, Share, Save to phone, Delete (to the Bin); in the Bin Restore and Delete forever.
 */
@Composable
fun ViewerScreen(vm: SnapViewModel, ui: UiState, route: Route.Viewer) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val model = vm.itemsModel(route.folderId, route.bin)
    val folders by vm.folders.collectAsStateWithLifecycle()
    val nowMs by rememberNow(30_000)
    val binDays = ui.settings?.binDays ?: 7u
    val storageLow = ui.status?.storageLow == true
    val linked = Texts.linkedCount(ui.devices)
    ReloadOnEvents(vm, model, ::itemsRelevant)
    var zoomed by remember { mutableStateOf(false) }
    var moving by rememberSaveable { mutableStateOf(false) }
    // The item the Delete forever question is about: fixed when it was asked, whatever the pager shows later.
    var foreverId by rememberSaveable { mutableStateOf<String?>(null) }
    LightSystemIcons()

    val rows = model.rows
    if (!model.loaded) {
        Box(Modifier.fillMaxSize().background(Color.Black), contentAlignment = Alignment.Center) { CircularProgressIndicator(color = Color.White) }
        return
    }
    // The last item left the list (deleted, moved out of the filter, restored): nothing more to show here.
    if (rows.isEmpty()) {
        LaunchedEffect(Unit) { vm.nav.back() }
        return
    }

    val pager = rememberPagerState(initialPage = rows.indexOfFirst { it.id == route.itemId }.coerceAtLeast(0)) { model.rows.size }
    LaunchedEffect(pager.currentPage) { zoomed = false }
    LaunchedEffect(pager, model) {
        androidx.compose.runtime.snapshotFlow { pager.currentPage }.collect { if (it >= model.rows.size - 5 && model.hasMore) model.loadMore() }
    }
    // Deleting the last page shrinks the list under the pager. The pager only moves back once it is laid out again,
    // so show the new last item now rather than nothing (an empty screen would never lay the pager out).
    val shown = pager.currentPage.coerceAtMost(rows.lastIndex)
    val item = rows[shown]

    fun openFile(i: ItemInfo) {
        scope.launch {
            val intent = vm.openIntent(i) ?: return@launch
            try {
                ctx.startActivity(intent)
            } catch (e: android.content.ActivityNotFoundException) {
                vm.say(FileRules.noAppText(i.ext))
            } catch (e: Exception) {
                vm.reportFailure("Open", e)
            }
        }
    }

    fun after(job: kotlinx.coroutines.Job) {
        scope.launch {
            job.join()
            model.reload()
        }
    }

    Column(Modifier.fillMaxSize().background(Color.Black)) {
        Row(Modifier.fillMaxWidth().statusBarsPadding().height(56.dp), verticalAlignment = Alignment.CenterVertically) {
            IconBtn(R.drawable.ic_arrow_back, "Back", { vm.nav.back() }, tint = Color.White)
            Column(Modifier.weight(1f)) {
                Text(item.name, color = Color.White, style = MaterialTheme.typography.titleMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text("${shown + 1} of ${rows.size}${if (model.hasMore) "+" else ""}", color = Color.White, style = MaterialTheme.typography.labelMedium)
            }
        }
        HorizontalPager(pager, Modifier.weight(1f).fillMaxWidth(), userScrollEnabled = !zoomed, key = { model.rows.getOrNull(it)?.id ?: it }) { page ->
            val row = model.rows.getOrNull(page)
            if (row != null) ViewerPage(row, active = page == pager.currentPage, bin = route.bin, storageLow = storageLow, several = linked > 1, onOpen = { openFile(row) }) { z -> if (page == pager.currentPage) zoomed = z }
        }
        Column(Modifier.fillMaxWidth().navigationBarsPadding().padding(bottom = 4.dp)) {
            val cell = UiLogic.cell(item, nowMs, binDays, route.bin, storageLow, linked)
            val folder = folders.firstOrNull { it.id == item.folderId }?.name
            Text(
                listOfNotNull(UiLogic.capturedAt(item.capturedAtMs, ZoneId.systemDefault()), folder, if (item.kind == MediaKind.FILE) null else FileRules.sizeText(item.size.toLong()), cell.label).joinToString("  -  "),
                color = Color.White, style = MaterialTheme.typography.bodySmall, textAlign = TextAlign.Center, modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp),
            )
            val held = UiLogic.fileHeld(item)
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceEvenly) {
                if (route.bin) {
                    ViewerAction(R.drawable.ic_restore_from_trash, "Restore", true) { after(vm.restore(item.id)) }
                    ViewerAction(R.drawable.ic_delete, "Delete forever", true) { foreverId = item.id }
                } else {
                    val folderKeep = item.state is ItemStateInfo.FolderKeep
                    ViewerAction(if (cell.kept) R.drawable.ic_star_fill1 else R.drawable.ic_star, if (cell.kept) "Kept" else "Keep", !folderKeep) { after(vm.keep(item.id, !item.pinned)) }
                    ViewerAction(R.drawable.ic_drive_file_move, "Move", true) { moving = true }
                    ViewerAction(R.drawable.ic_share, "Share", held) { scope.launch { vm.shareIntent(item)?.let { i -> ctx.startActivity(i) } } }
                    // Only photos and videos go to the phone gallery. A file has no Save: its one Open is the big button on its page (its size is there too).
                    if (item.kind != MediaKind.FILE) ViewerAction(R.drawable.ic_download, "Save", held) { vm.saveToGallery(item) }
                    ViewerAction(R.drawable.ic_delete, "Delete", true) { after(vm.delete(item.id)) } // Shell offers Undo once it succeeded
                }
            }
        }
    }

    foreverId?.let { id ->
        ForeverDialog(
            Texts.DELETE_FOREVER_TITLE, Texts.DELETE_FOREVER_TEXT, "Delete forever",
            onConfirm = {
                foreverId = null
                after(vm.purge(id))
            },
            onDismiss = { foreverId = null },
        )
    }

    if (moving) {
        FolderSheet(
            title = "Move to", folders = folders.filter { it.id != item.folderId }, currentId = null, autoLabel = null,
            onPick = { id ->
                moving = false
                if (id != null) after(vm.move(item.id, id))
            },
            onDismiss = { moving = false },
        )
    }
}

/** One of the action row's equal-width slots. Above 1.5x font size the words go (icon + spoken label only): five words no longer fit. */
@Composable
private fun RowScope.ViewerAction(icon: Int, label: String, enabled: Boolean, onClick: () -> Unit) {
    val tint = Color.White.copy(alpha = if (enabled) 1f else 0.4f)
    val words = LocalDensity.current.fontScale <= 1.5f
    Column(
        Modifier.weight(1f).defaultMinSize(minHeight = 56.dp).clickable(enabled = enabled, role = Role.Button, onClick = onClick).padding(horizontal = 2.dp, vertical = 4.dp)
            .semantics { contentDescription = label },
        horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center,
    ) {
        Ico(icon, null, tint = tint)
        if (words) Text(label, color = tint, style = MaterialTheme.typography.labelMedium, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

@Composable
private fun ViewerPage(item: ItemInfo, active: Boolean, bin: Boolean, storageLow: Boolean, several: Boolean, onOpen: () -> Unit, onZoomed: (Boolean) -> Unit) {
    val note = UiLogic.fileNote(item.local, bin, storageLow, several)
    val path = item.path
    when {
        item.kind == MediaKind.FILE -> FilePage(item, note ?: if (path == null) "FILE MISSING on this phone" else null, bin, onOpen)
        note != null || path == null -> NoFile(item, note ?: "FILE MISSING on this phone")
        item.kind == MediaKind.VIDEO -> if (active) VideoPlayer(path) else ThumbImage(item.thumbPath, "Video", Modifier.fillMaxSize(), ContentScale.Fit)
        else -> PhotoPage(item, path, onZoomed)
    }
}

/** A file that is not a photo or video: type icon, real name, "PDF, 1.2 MB" and Open. In the Bin: Restore first. */
@Composable
private fun FilePage(item: ItemInfo, note: String?, bin: Boolean, onOpen: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
        Ico(fileIcon(item.ext), null, tint = Color.White, size = 72)
        Spacer(Modifier.height(12.dp))
        Text(FileRules.displayName(item.name, item.ext), color = Color.White, style = MaterialTheme.typography.titleLarge, textAlign = TextAlign.Center, maxLines = 4, overflow = TextOverflow.Ellipsis)
        Text(FileRules.meta(item.ext, item.size.toLong()), color = Color.White, style = MaterialTheme.typography.bodyMedium, textAlign = TextAlign.Center)
        Spacer(Modifier.height(20.dp))
        when {
            note != null -> Text(note, color = Color.White, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
            bin -> Text("Restore it to open it", color = Color.White, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
            UiLogic.canOpen(item, bin) -> ActionButton(onClick = onOpen, modifier = Modifier.defaultMinSize(minHeight = 56.dp)) { Text("Open") }
            // Binned since this list was built: the provider serves items/ only, so there is nothing to open.
            else -> Text("In the Bin", color = Color.White, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
        }
    }
}

/** The file is not here: the thumbnail, dimmed, with the reason in words. */
@Composable
private fun NoFile(item: ItemInfo, note: String) {
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        Box(Modifier.fillMaxSize().graphicsLayer { alpha = 0.35f }) { ThumbImage(item.thumbPath, null, Modifier.fillMaxSize(), ContentScale.Fit) }
        Text(
            note, color = Color.White, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center,
            modifier = Modifier.background(Scrim, MaterialTheme.shapes.medium).padding(horizontal = 16.dp, vertical = 10.dp),
        )
    }
}

private sealed interface PhotoState {
    data object Loading : PhotoState
    class Ready(val bitmap: ImageBitmap) : PhotoState
    data object Failed : PhotoState
}

@Composable
private fun PhotoPage(item: ItemInfo, path: String, onZoomed: (Boolean) -> Unit) {
    val ctx = LocalContext.current
    // Full-size decode off the main thread, capped so a 50 MP photo never exhausts memory. A file that cannot be
    // decoded (or runs out of memory) ends in Failed, never in a spinner that never stops or a crash.
    val state by produceState<PhotoState>(PhotoState.Loading, UiLogic.photoDecodeKey(path, item.size, item.thumbPath)) {
        value = withContext(Dispatchers.IO) {
            try {
                Thumbs.imageBitmap(File(path), 2048)?.let { PhotoState.Ready(it.asImageBitmap()) } ?: PhotoState.Failed
            } catch (e: Exception) {
                KLog.log(ctx, "photo decode failed: $e")
                PhotoState.Failed
            } catch (e: OutOfMemoryError) {
                KLog.log(ctx, "photo decode failed: out of memory")
                PhotoState.Failed
            }
        }
    }
    when (val s = state) {
        PhotoState.Loading -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator(color = Color.White) }
        is PhotoState.Ready -> ZoomableImage(s.bitmap, item.name, onZoomed)
        PhotoState.Failed -> NoFile(item, "CANNOT SHOW THIS PHOTO - Share or Save it instead")
    }
}

/**
 * Pinch, drag when zoomed, double tap to zoom in and out. A one-finger drag at 1x is left alone so the pager
 * can swipe to the next item.
 */
@Composable
private fun ZoomableImage(bitmap: ImageBitmap, desc: String, onZoomed: (Boolean) -> Unit) {
    var scale by remember { mutableFloatStateOf(1f) }
    var offset by remember { mutableStateOf(Offset.Zero) }
    var size by remember { mutableStateOf(IntSize.Zero) }
    Box(
        Modifier.fillMaxSize().onSizeChanged { size = it }
            .pointerInput(Unit) {
                detectTapGestures(onDoubleTap = {
                    scale = if (scale > 1f) 1f else 2.5f
                    offset = Offset.Zero
                    onZoomed(scale > 1f)
                })
            }
            .pointerInput(Unit) {
                awaitEachGesture {
                    awaitFirstDown(requireUnconsumed = false)
                    do {
                        val ev = awaitPointerEvent()
                        if (ev.changes.size > 1 || scale > 1f) {
                            scale = (scale * ev.calculateZoom()).coerceIn(1f, 6f)
                            val pan = ev.calculatePan()
                            val mx = UiLogic.panLimit(scale, size.width.toFloat())
                            val my = UiLogic.panLimit(scale, size.height.toFloat())
                            offset = if (scale <= 1f) Offset.Zero else Offset((offset.x + pan.x).coerceIn(-mx, mx), (offset.y + pan.y).coerceIn(-my, my))
                            ev.changes.forEach { if (it.positionChanged()) it.consume() }
                            onZoomed(scale > 1f)
                        }
                    } while (ev.changes.any { it.pressed })
                }
            },
    ) {
        Image(
            bitmap, desc, contentScale = ContentScale.Fit,
            modifier = Modifier.fillMaxSize().graphicsLayer { scaleX = scale; scaleY = scale; translationX = offset.x; translationY = offset.y },
        )
    }
}

/**
 * Decoder fallback on: if a phone's first-choice decoder cannot be set up, the next one is tried (often the software decoder).
 * Debug builds also skip the Android emulator's own "goldfish" decoders: on the emulator image used for QA they fail for any
 * third-party app (SELinux denies the codec service the app's shared-memory queue),
 * so without this no recorded video could be played there. Release builds never touch the decoder list.
 */
@androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)
private fun renderersFactory(ctx: android.content.Context): androidx.media3.exoplayer.DefaultRenderersFactory {
    val f = androidx.media3.exoplayer.DefaultRenderersFactory(ctx).setEnableDecoderFallback(true)
    if (BuildConfig.DEBUG) {
        f.setMediaCodecSelector { mime, secure, tunneling ->
            androidx.media3.exoplayer.mediacodec.MediaCodecSelector.DEFAULT.getDecoderInfos(mime, secure, tunneling).filterNot { it.name.contains("goldfish") }
        }
    }
    return f
}

/** Media3 player for the current page only: created when the page settles, released when it leaves. */
@androidx.annotation.OptIn(androidx.media3.common.util.UnstableApi::class)
@Composable
private fun VideoPlayer(path: String) {
    val ctx = LocalContext.current
    val player = remember(path) {
        ExoPlayer.Builder(ctx, renderersFactory(ctx)).build().apply {
            setMediaItem(MediaItem.fromUri(Uri.fromFile(File(path))))
            prepare()
        }
    }
    var failed by remember(path) { mutableStateOf(false) }
    DisposableEffect(player) {
        val l = object : Player.Listener {
            override fun onPlayerError(e: PlaybackException) {
                failed = true
                // The full codec text is long: it goes to the log, the screen gets one short line.
                KLog.log(ctx, "video playback failed: ${e.errorCodeName}: ${e.message}")
            }
        }
        player.addListener(l)
        onDispose {
            player.removeListener(l)
            player.release()
        }
    }
    LifecycleEventEffect(Lifecycle.Event.ON_STOP) { player.pause() }
    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        AndroidView({ c -> PlayerView(c).apply { this.player = player; useController = true } }, Modifier.fillMaxSize(), update = { it.player = player })
        if (failed) {
            Text(
                "CANNOT PLAY THIS VIDEO - Share or Save it instead", color = Color.White, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center,
                modifier = Modifier.background(Scrim, MaterialTheme.shapes.medium).padding(horizontal = 16.dp, vertical = 10.dp),
            )
        }
    }
}
