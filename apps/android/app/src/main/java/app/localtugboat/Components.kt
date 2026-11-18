package app.localtugboat

import android.graphics.BitmapFactory
import android.util.LruCache
import androidx.annotation.DrawableRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import uniffi.tugboat_ffi.FolderInfo
import uniffi.tugboat_ffi.SyncStatus
import androidx.compose.ui.text.style.TextOverflow

/** Minimum touch target everywhere. */
val Touch = 48.dp

/** Monochrome Material Symbols icon from res/drawable. [desc] null = decorative (the text beside it says it). */
@Composable
fun Ico(@DrawableRes id: Int, desc: String?, modifier: Modifier = Modifier, tint: Color = MaterialTheme.colorScheme.onSurface, size: Int = 24) {
    Icon(painterResource(id), desc, modifier.size(size.dp), tint)
}

@Composable
fun IconBtn(@DrawableRes id: Int, desc: String, onClick: () -> Unit, modifier: Modifier = Modifier, tint: Color = MaterialTheme.colorScheme.onSurface, enabled: Boolean = true) {
    IconButton(onClick = onClick, modifier = modifier.size(Touch), enabled = enabled) { Icon(painterResource(id), desc, tint = tint) }
}

/** A clock that ticks every [everyMs], so "Synced 10s ago" and "Expires in 5h" stay true on screen. */
@Composable
fun rememberNow(everyMs: Long = 5_000): State<Long> {
    val now = remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(everyMs) {
        while (true) {
            delay(everyMs)
            now.longValue = System.currentTimeMillis()
        }
    }
    return now
}

/** Status as a tinted block: icon + text + optional action. No coloured edge stripe. Blockers are red. */
@Composable
fun BannerCard(b: UiLogic.BannerUi, onAction: () -> Unit, modifier: Modifier = Modifier) {
    val blocker = b.level == UiLogic.Level.BLOCKER
    val bg = if (blocker) MaterialTheme.colorScheme.errorContainer else MaterialTheme.colorScheme.tertiaryContainer
    val fg = if (blocker) MaterialTheme.colorScheme.onErrorContainer else MaterialTheme.colorScheme.onTertiaryContainer
    Surface(color = bg, contentColor = fg, shape = MaterialTheme.shapes.medium, modifier = modifier.fillMaxWidth()) {
        Row(Modifier.defaultMinSize(minHeight = Touch).padding(start = 12.dp, end = 4.dp, top = 4.dp, bottom = 4.dp), verticalAlignment = Alignment.CenterVertically) {
            Ico(R.drawable.ic_warning, null, tint = fg)
            Spacer(Modifier.width(10.dp))
            Text(b.text, style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f).padding(vertical = 6.dp))
            if (b.action != null) TextButton(onClick = onAction, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text(b.action, color = fg) }
        }
    }
}

@Composable
fun BannerStack(banners: List<UiLogic.BannerUi>, onAction: (UiLogic.BannerUi) -> Unit, modifier: Modifier = Modifier, max: Int = Int.MAX_VALUE) {
    if (banners.isEmpty()) return
    Column(modifier, verticalArrangement = Arrangement.spacedBy(6.dp)) {
        banners.take(max).forEach { BannerCard(it, { onAction(it) }) }
        if (banners.size > max) Text("+${banners.size - max} more in Settings", style = MaterialTheme.typography.labelMedium, color = Color.White, modifier = Modifier.background(Scrim, MaterialTheme.shapes.small).padding(horizontal = 8.dp, vertical = 2.dp))
    }
}

/** Dot + words. The words carry the meaning; the dot colour is only a hint. */
@Composable
fun SyncLine(status: SyncStatus?, nowMs: Long, modifier: Modifier = Modifier, onScrim: Boolean = false) {
    val cs = MaterialTheme.colorScheme
    val tone = UiLogic.tone(status)
    // On the camera scrim the dot colours must read on near-black; elsewhere they follow the theme.
    val dot = when (tone) {
        UiLogic.Tone.OK -> if (onScrim) c(ScrimDots.OK) else c(LocalPalette.current.ok)
        UiLogic.Tone.BUSY -> if (onScrim) c(ScrimDots.BUSY) else cs.primary
        UiLogic.Tone.WARN -> if (onScrim) c(ScrimDots.WARN) else cs.tertiary
        UiLogic.Tone.BAD -> if (onScrim) c(ScrimDots.BAD) else cs.error
    }
    val text = if (status == null) "Starting" else Texts.cameraSync(status, nowMs)
    Row(modifier.defaultMinSize(minHeight = 24.dp), verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(8.dp).clip(CircleShape).background(dot))
        Spacer(Modifier.width(6.dp))
        Text(text, style = MaterialTheme.typography.labelMedium, color = if (onScrim) Color.White else cs.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

private object ThumbCache {
    private const val MAX_KB = 32 * 1024
    val lru = object : LruCache<String, ImageBitmap>(MAX_KB) {
        override fun sizeOf(key: String, value: ImageBitmap) = (value.width * value.height * 4) / 1024
    }
}

/** A held thumbnail (small JPEG made on the phone), or a placeholder when there is none. Cached, decoded off the main thread. */
@Composable
fun ThumbImage(path: String?, desc: String?, modifier: Modifier = Modifier, scale: ContentScale = ContentScale.Crop) {
    val bmp by produceState(path?.let { ThumbCache.lru.get(it) }, path) {
        value = if (path == null) null else ThumbCache.lru.get(path) ?: withContext(Dispatchers.IO) {
            BitmapFactory.decodeFile(path)?.asImageBitmap()?.also { ThumbCache.lru.put(path, it) }
        }
    }
    val b = bmp
    if (b != null) {
        Image(b, desc, modifier, contentScale = scale)
    } else {
        Box(modifier.background(MaterialTheme.colorScheme.surfaceVariant), contentAlignment = Alignment.Center) {
            Ico(R.drawable.ic_broken_image, desc, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@DrawableRes
fun fileIcon(ext: String): Int = when (FileRules.glyph(ext)) {
    FileGlyph.DOCUMENT -> R.drawable.ic_file_doc
    FileGlyph.ARCHIVE -> R.drawable.ic_file_archive
    FileGlyph.AUDIO -> R.drawable.ic_file_audio
    FileGlyph.OTHER -> R.drawable.ic_file
}

/** What a file that is not a photo or video shows where a thumbnail would be: type icon, name, "PDF, 1.2 MB". */
@Composable
fun FileFace(item: uniffi.tugboat_ffi.ItemInfo, modifier: Modifier = Modifier) {
    val cs = MaterialTheme.colorScheme
    Column(
        modifier.background(cs.surfaceVariant).padding(start = 6.dp, end = 6.dp, top = 6.dp, bottom = 30.dp),
        horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center,
    ) {
        Ico(fileIcon(item.ext), null, tint = cs.onSurfaceVariant, size = 28)
        Text(
            FileRules.displayName(item.name, item.ext), style = MaterialTheme.typography.labelMedium, color = cs.onSurface,
            maxLines = 2, overflow = TextOverflow.Ellipsis, textAlign = androidx.compose.ui.text.style.TextAlign.Center,
        )
        Text(FileRules.meta(item.ext, item.size.toLong()), style = MaterialTheme.typography.labelSmall, color = cs.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

/**
 * Bottom sheet to pick a folder. [autoLabel] non-null adds a first row (the rule-based choice).
 * [currentId] gets a check mark and a "Current" word, so the choice is not colour or icon alone.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun FolderSheet(title: String, folders: List<FolderInfo>, currentId: String?, autoLabel: String?, onPick: (String?) -> Unit, onDismiss: () -> Unit) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(Modifier.navigationBarsPadding()) {
            Text(title, style = MaterialTheme.typography.titleMedium, modifier = Modifier.padding(horizontal = 24.dp, vertical = 8.dp))
            LazyColumn {
                if (autoLabel != null) item("auto") { FolderRow(autoLabel, null, currentId == null, { onPick(null) }) }
                items(folders, key = { it.id }) { f -> FolderRow(f.name, UiLogic.retention(f.retentionSecs), f.id == currentId) { onPick(f.id) } }
            }
        }
    }
}

@Composable
private fun FolderRow(name: String, sub: String?, current: Boolean, onClick: () -> Unit) {
    Row(
        Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp).clickable(role = Role.Button, onClick = onClick).padding(horizontal = 24.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Ico(R.drawable.ic_folder, null, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(16.dp))
        Column(Modifier.weight(1f)) {
            Text(name, style = MaterialTheme.typography.bodyLarge)
            if (sub != null) Text(sub, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        if (current) {
            Text("Current", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
            Spacer(Modifier.width(6.dp))
            Ico(R.drawable.ic_check, null, tint = MaterialTheme.colorScheme.primary)
        }
    }
}

/** Centred message with an optional action: empty, error and permission-denied states. */
@Composable
fun Notice(text: String, modifier: Modifier = Modifier, action: String? = null, onAction: () -> Unit = {}, blocker: Boolean = false, art: Boolean = false) {
    Column(modifier.fillMaxWidth().padding(32.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
        if (art) {
            TugboatArt(72.dp)
            Spacer(Modifier.size(16.dp))
        }
        Text(
            text, style = MaterialTheme.typography.bodyLarge, textAlign = androidx.compose.ui.text.style.TextAlign.Center,
            color = if (blocker) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
        )
        if (action != null) {
            Spacer(Modifier.size(12.dp))
            androidx.compose.material3.FilledTonalButton(onClick = onAction, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text(action) }
        }
    }
}

/** Marks a node as one labelled unit for screen readers (a thumbnail cell says its label, not each part). */
fun Modifier.described(text: String): Modifier = semantics(mergeDescendants = true) { contentDescription = text }

/** Title row of a sub-screen: back arrow, title, optional action icons. Handles the status bar inset. */
@Composable
fun ScreenBar(title: String, onBack: (() -> Unit)?, modifier: Modifier = Modifier, actions: @Composable RowScope.() -> Unit = {}) {
    Row(modifier.fillMaxWidth().statusBarsPadding().height(56.dp).padding(horizontal = 4.dp), verticalAlignment = Alignment.CenterVertically) {
        if (onBack != null) IconBtn(R.drawable.ic_arrow_back, "Back", onBack) else Spacer(Modifier.width(12.dp))
        Text(title, style = MaterialTheme.typography.titleLarge, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
        actions()
    }
}
