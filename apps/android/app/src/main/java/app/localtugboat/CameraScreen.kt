package app.localtugboat

import android.annotation.SuppressLint
import android.Manifest
import android.content.pm.PackageManager
import android.os.SystemClock
import android.view.GestureDetector
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.FocusMeteringAction
import androidx.camera.core.ImageCapture
import androidx.camera.core.ImageCaptureException
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.video.FileOutputOptions
import androidx.camera.video.Quality
import androidx.camera.video.QualitySelector
import androidx.camera.video.Recorder
import androidx.camera.video.Recording
import androidx.camera.video.VideoCapture
import androidx.camera.video.VideoRecordEvent
import androidx.camera.view.PreviewView
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsTopHeight
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import androidx.lifecycle.Observer
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.tugboat_ffi.GeoFix
import androidx.compose.ui.text.style.TextOverflow

/** Zoom numbers the lens chips and the pinch gesture share. */
private class ZoomInfo(val ratio: Float, val min: Float, val max: Float)

/**
 * The home screen. Full-bleed preview, shutter, flip, flash, zoom chips, tap to focus, the folder chip and the
 * sync line. A shot is written to a file by CameraX and handed to the core off the main thread: the shutter
 * never waits for the database.
 */
@SuppressLint("ClickableViewAccessibility")
@Composable
fun CameraScreen(vm: SnapViewModel, ui: UiState, banners: List<UiLogic.BannerUi>, onBanner: (UiLogic.BannerUi) -> Unit, actions: ShellActions) {
    val ctx = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val haptic = LocalHapticFeedback.current
    val main = remember { ContextCompat.getMainExecutor(ctx) }
    val nowMs by rememberNow(5_000)
    val folders by vm.folders.collectAsStateWithLifecycle()
    val latest by vm.latest.collectAsStateWithLifecycle()
    val hardDenied by vm.hardDenied.collectAsStateWithLifecycle()
    val nextShot by ctx.core.nextShotFolder.collectAsStateWithLifecycle()
    val perms = ui.perms
    val cameraOk = perms?.camera == true
    val micOk = perms?.microphone == true

    var video by rememberSaveable { mutableStateOf(false) }
    var front by rememberSaveable { mutableStateOf(false) }
    var flash by rememberSaveable { mutableStateOf(UiLogic.Flash.OFF) }
    var showFolders by rememberSaveable { mutableStateOf(false) }

    var camera by remember { mutableStateOf<Camera?>(null) }
    var imageCapture by remember { mutableStateOf<ImageCapture?>(null) }
    var videoCapture by remember { mutableStateOf<VideoCapture<Recorder>?>(null) }
    var cameraError by remember { mutableStateOf<String?>(null) }
    var canFlip by remember { mutableStateOf(false) }
    var hasFlash by remember { mutableStateOf(false) }
    var zoom by remember { mutableStateOf<ZoomInfo?>(null) }
    var recording by remember { mutableStateOf<Recording?>(null) }
    val recordMs = remember { mutableLongStateOf(0L) }
    var focusAt by remember { mutableStateOf<Offset?>(null) }
    var shutterFlash by remember { mutableStateOf(false) }
    var target by remember { mutableStateOf<Target?>(null) }
    val tracker = remember { LocationTracker(ctx) }

    val previewView = remember { PreviewView(ctx).apply { scaleType = PreviewView.ScaleType.FILL_CENTER } }
    val cameraNow by rememberUpdatedState(camera)

    LightSystemIcons()

    // Location only while this screen is visible (while-in-use permission, never background).
    LifecycleResumeEffect(tracker) {
        tracker.start()?.let { KLog.log(ctx, "location not started: $it") }
        onPauseOrDispose { tracker.stop() }
    }

    // What the chip shows is what the next shot gets: recomputed when the pick changes and every few seconds.
    LaunchedEffect(nextShot, folders) {
        var lastError: String? = null
        while (true) {
            try {
                target = withContext(Dispatchers.IO) { Capture.target(ctx, tracker.current()) }
                lastError = null
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                target = null
                if (e.message != lastError) KLog.log(ctx, "chip target: ${e.message}")
                lastError = e.message
            }
            delay(3_000)
        }
    }

    // Bind CameraX. Photo mode: preview + ImageCapture. Video mode: preview + VideoCapture. Rebinds on a change.
    DisposableEffect(owner, cameraOk, front, video) {
        var provider: ProcessCameraProvider? = null
        var zoomLive: androidx.lifecycle.LiveData<androidx.camera.core.ZoomState>? = null
        val zoomObserver = Observer<androidx.camera.core.ZoomState> { zoom = ZoomInfo(it.zoomRatio, it.minZoomRatio, it.maxZoomRatio) }
        if (cameraOk) {
            val future = ProcessCameraProvider.getInstance(ctx)
            future.addListener({
                try {
                    val p = future.get()
                    provider = p
                    canFlip = p.hasCamera(CameraSelector.DEFAULT_BACK_CAMERA) && p.hasCamera(CameraSelector.DEFAULT_FRONT_CAMERA)
                    val selector = if (front) CameraSelector.DEFAULT_FRONT_CAMERA else CameraSelector.DEFAULT_BACK_CAMERA
                    if (!p.hasCamera(selector)) throw IllegalStateException(if (front) "No front camera on this phone" else "No back camera on this phone")
                    val preview = Preview.Builder().build().also { it.surfaceProvider = previewView.surfaceProvider }
                    p.unbindAll()
                    val c = if (!video) {
                        val ic = ImageCapture.Builder().setCaptureMode(ImageCapture.CAPTURE_MODE_MINIMIZE_LATENCY).build()
                        imageCapture = ic
                        videoCapture = null
                        p.bindToLifecycle(owner, selector, preview, ic)
                    } else {
                        val vc = VideoCapture.withOutput(Recorder.Builder().setQualitySelector(QualitySelector.from(Quality.HD, androidx.camera.video.FallbackStrategy.lowerQualityOrHigherThan(Quality.SD))).build())
                        videoCapture = vc
                        imageCapture = null
                        p.bindToLifecycle(owner, selector, preview, vc)
                    }
                    camera = c
                    hasFlash = c.cameraInfo.hasFlashUnit()
                    zoomLive = c.cameraInfo.zoomState.also { it.observe(owner, zoomObserver) }
                    cameraError = null
                } catch (e: Exception) {
                    camera = null
                    cameraError = e.message ?: e.javaClass.simpleName
                    ctx.core.reportError("camera: $cameraError")
                }
            }, main)
        }
        onDispose {
            // A recording in progress is stopped so its file is finished and kept (the finalize event hands it to the core).
            recording?.stop()
            zoomLive?.removeObserver(zoomObserver)
            provider?.unbindAll()
            camera = null
            zoom = null
        }
    }

    // Flash for photos, torch for video.
    LaunchedEffect(flash, camera, video) {
        imageCapture?.flashMode = when (flash) {
            UiLogic.Flash.OFF -> ImageCapture.FLASH_MODE_OFF
            UiLogic.Flash.AUTO -> ImageCapture.FLASH_MODE_AUTO
            UiLogic.Flash.ON -> ImageCapture.FLASH_MODE_ON
        }
        camera?.cameraControl?.enableTorch(video && flash == UiLogic.Flash.ON)
    }

    // Pinch to zoom, tap to focus.
    DisposableEffect(previewView) {
        val scale = ScaleGestureDetector(ctx, object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
            override fun onScale(d: ScaleGestureDetector): Boolean {
                val c = cameraNow ?: return true
                val z = c.cameraInfo.zoomState.value ?: return true
                c.cameraControl.setZoomRatio((z.zoomRatio * d.scaleFactor).coerceIn(z.minZoomRatio, z.maxZoomRatio))
                return true
            }
        })
        val tap = GestureDetector(ctx, object : GestureDetector.SimpleOnGestureListener() {
            override fun onSingleTapUp(e: MotionEvent): Boolean {
                val c = cameraNow ?: return true
                val point = previewView.meteringPointFactory.createPoint(e.x, e.y)
                c.cameraControl.startFocusAndMetering(FocusMeteringAction.Builder(point).build())
                focusAt = Offset(e.x, e.y)
                return true
            }
        })
        previewView.setOnTouchListener { v, ev ->
            scale.onTouchEvent(ev)
            if (!scale.isInProgress) tap.onTouchEvent(ev)
            if (ev.actionMasked == MotionEvent.ACTION_UP) v.performClick()
            true
        }
        onDispose { previewView.setOnTouchListener(null) }
    }
    LaunchedEffect(focusAt) {
        if (focusAt != null) {
            delay(900)
            focusAt = null
        }
    }
    LaunchedEffect(shutterFlash) {
        if (shutterFlash) {
            delay(90)
            shutterFlash = false
        }
    }

    fun newFile(prefix: String, ext: String) = File(Capture.shotsDir(ctx), prefix + SimpleDateFormat("yyyyMMdd_HHmmss_SSS", Locale.ROOT).format(Date()) + "." + ext)

    fun geoNow(): GeoFix? = tracker.current()?.takeIf { UiLogic.usableFix(it.accuracyM, it.ageMs) }?.let { GeoFix(it.lat, it.lon, it.accuracyM) }

    fun takePhoto() {
        val ic = imageCapture ?: return
        val file = newFile("IMG_", "jpg")
        val folderId = target?.folderId
        val geo = geoNow()
        val at = System.currentTimeMillis()
        shutterFlash = true
        haptic.performHapticFeedback(HapticFeedbackType.LongPress)
        Capture.shotTaken(ctx)
        ic.takePicture(ImageCapture.OutputFileOptions.Builder(file).build(), main, object : ImageCapture.OnImageSavedCallback {
            override fun onImageSaved(r: ImageCapture.OutputFileResults) = handOff(vm, ctx, file, folderId, at, geo, false)
            override fun onError(e: ImageCaptureException) {
                file.delete()
                ctx.core.reportError("photo: ${e.message}")
                vm.say("Photo failed: ${e.message}")
            }
        })
    }

    @SuppressLint("MissingPermission") // withAudioEnabled is called only after the checkSelfPermission just above it
    fun toggleRecording() {
        recording?.let {
            it.stop()
            return
        }
        val vc = videoCapture ?: return
        val file = newFile("VID_", "mp4")
        val folderId = target?.folderId
        val geo = geoNow()
        val at = System.currentTimeMillis()
        var pending = vc.output.prepareRecording(ctx, FileOutputOptions.Builder(file).build())
        if (ContextCompat.checkSelfPermission(ctx, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) pending = pending.withAudioEnabled()
        haptic.performHapticFeedback(HapticFeedbackType.LongPress)
        Capture.shotTaken(ctx)
        recordMs.longValue = 0
        recording = pending.start(main) { ev ->
            when (ev) {
                is VideoRecordEvent.Status -> recordMs.longValue = ev.recordingStats.recordedDurationNanos / 1_000_000
                is VideoRecordEvent.Finalize -> {
                    recording = null
                    if (ev.hasError()) {
                        // The file is still handed over when it has content: the core checks it, a recording is never thrown away here.
                        ctx.core.reportError("video: error ${ev.error} ${ev.cause?.message.orEmpty()}")
                        vm.say("Video stopped: ${videoErrorText(ev.error)}")
                    }
                    if (file.length() > 0) handOff(vm, ctx, file, folderId, at, geo, true) else file.delete()
                }
                else -> {}
            }
        }
    }

    fun chooseVideo(on: Boolean) {
        video = on
        flash = UiLogic.Flash.OFF
        if (on && !micOk && PermKind.MICROPHONE !in hardDenied && !Prefs.of(ctx).getBoolean(PREF_MIC_ASKED, false)) {
            Prefs.of(ctx).edit { putBoolean(PREF_MIC_ASKED, true) }
            actions.request(PermKind.MICROPHONE)
        }
    }

    Box(Modifier.fillMaxSize().background(Color.Black)) {
        if (cameraOk && cameraError == null) AndroidView({ previewView }, Modifier.fillMaxSize())
        if (shutterFlash) Box(Modifier.fillMaxSize().background(Color.White.copy(alpha = 0.35f)))

        focusAt?.let { p ->
            val d = LocalDensity.current
            // White ring with a dark edge on both sides: a white ring alone vanishes on a bright scene.
            Box(
                Modifier.offset { IntOffset((p.x - with(d) { 38.dp.toPx() }).toInt(), (p.y - with(d) { 38.dp.toPx() }).toInt()) }.size(76.dp)
                    .border(1.dp, Scrim, CircleShape).padding(1.dp).border(2.dp, Color.White, CircleShape).padding(2.dp).border(1.dp, Scrim, CircleShape),
            )
        }

        if (!cameraOk || cameraError != null) {
            CameraNotice(
                if (!cameraOk) "CAMERA OFF - allow the camera to take photos" else "CAMERA UNAVAILABLE - $cameraError",
                if (!cameraOk) (if (PermKind.CAMERA in hardDenied) "Open settings" else "Allow camera") else null,
            ) { if (PermKind.CAMERA in hardDenied) actions.openAppSettings() else actions.request(PermKind.CAMERA) }
        }

        // Darkens the strip behind the clock, signal and battery: white icons over a bright scene are invisible otherwise.
        Box(
            Modifier.fillMaxWidth().windowInsetsTopHeight(WindowInsets.statusBars)
                .background(Brush.verticalGradient(0f to Scrim, 0.85f to Scrim, 1f to Color.Transparent)),
        )

        Column(Modifier.fillMaxSize()) {
            // ---- top ----
            Row(Modifier.fillMaxWidth().statusBarsPadding().padding(horizontal = 8.dp, vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                if (hasFlash && cameraOk) {
                    val (icon, desc) = when (flash) {
                        UiLogic.Flash.OFF -> R.drawable.ic_flash_off to "Flash off"
                        UiLogic.Flash.AUTO -> R.drawable.ic_flash_auto to "Flash auto"
                        UiLogic.Flash.ON -> R.drawable.ic_flash_on to (if (video) "Torch on" else "Flash on")
                    }
                    ScrimIconBtn(icon, desc, { flash = UiLogic.nextFlash(flash, video) })
                } else {
                    Spacer(Modifier.size(Touch))
                }
                Spacer(Modifier.weight(1f))
                ScrimIconBtn(R.drawable.ic_settings, "Settings", { vm.nav.go(Route.Settings) })
            }
            Column(Modifier.padding(horizontal = 12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                BannerStack(banners, onBanner, max = 2)
                SyncLine(ui.status, nowMs, onScrim = true, modifier = Modifier.background(Scrim, RoundedCornerShape(50)).padding(horizontal = 10.dp, vertical = 2.dp))
            }
            Spacer(Modifier.weight(1f))

            // ---- bottom, in reach of the thumb ----
            Column(Modifier.fillMaxWidth().navigationBarsPadding().padding(bottom = 12.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(12.dp)) {
                // Where the next shot goes, changed per shot: in thumb reach, not under the status bar.
                FolderChip(UiLogic.chip(target), enabled = recording == null) { showFolders = true }
                val z = zoom
                if (z != null && recording == null) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.background(Scrim, RoundedCornerShape(50)).padding(4.dp)) {
                        UiLogic.lensChips(z.min, z.max).forEach { r ->
                            val on = Math.abs(z.ratio - r) < 0.05f
                            ZoomChip(UiLogic.zoomLabel(r), on) { cameraNow?.cameraControl?.setZoomRatio(r.coerceIn(z.min, z.max)) }
                        }
                    }
                }
                if (recording != null) {
                    Row(Modifier.background(Scrim, RoundedCornerShape(50)).padding(horizontal = 14.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
                        Box(Modifier.size(10.dp).clip(CircleShape).background(Color(0xFFFF6B5E)))
                        Spacer(Modifier.width(8.dp))
                        Text("REC ${UiLogic.clock(recordMs.longValue)}", color = Color.White, style = MaterialTheme.typography.titleMedium)
                    }
                } else {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
                        ModeChip("Photo", !video) { chooseVideo(false) }
                        ModeChip("Video", video) { chooseVideo(true) }
                        if (video && !micOk) {
                            Text(
                                "NO SOUND", color = Color.White, style = MaterialTheme.typography.labelMedium,
                                modifier = Modifier.defaultMinSize(minHeight = Touch).clickable(role = Role.Button) { if (PermKind.MICROPHONE in hardDenied) actions.openAppSettings() else actions.request(PermKind.MICROPHONE) }
                                    .background(Scrim, RoundedCornerShape(50)).padding(horizontal = 12.dp, vertical = 14.dp),
                            )
                        }
                    }
                }
                Row(Modifier.fillMaxWidth().padding(horizontal = 24.dp), verticalAlignment = Alignment.CenterVertically) {
                    Box(Modifier.weight(1f), contentAlignment = Alignment.CenterStart) {
                        LastShot(latest) { vm.nav.go(Route.Gallery(null)) }
                    }
                    Shutter(video, recording != null, cameraOk && camera != null) { if (video) toggleRecording() else takePhoto() }
                    Box(Modifier.weight(1f), contentAlignment = Alignment.CenterEnd) {
                        if (canFlip && recording == null) ScrimIconBtn(R.drawable.ic_cameraswitch, if (front) "Use back camera" else "Use front camera", { front = !front })
                    }
                }
            }
        }
    }

    if (showFolders) {
        FolderSheet(
            title = "Next shot goes to", folders = folders, currentId = nextShot, autoLabel = "Automatic (by place)",
            onPick = { id ->
                ctx.core.nextShotFolder.value = id
                showFolders = false
            },
            onDismiss = { showFolders = false },
        )
    }
}

private const val PREF_MIC_ASKED = "mic_asked"

/** Hand a finished file to the core on the process scope: it carries on if the user leaves the screen. */
private fun handOff(vm: SnapViewModel, ctx: android.content.Context, file: File, folderId: String?, atMs: Long, geo: GeoFix?, video: Boolean) {
    ctx.core.scope.launch {
        try {
            Capture.add(ctx, file, file.name, folderId, atMs, geo, imported = false, video = video)
            vm.refreshLatest()
        } catch (e: Exception) {
            // The file stays in shots/ and Capture.recover retries it: nothing is lost.
            ctx.core.reportError("add ${file.name}: ${e.message}")
            vm.say("Saved on this phone, not synced yet: ${e.message}")
        }
    }
}

private fun videoErrorText(code: Int) = when (code) {
    VideoRecordEvent.Finalize.ERROR_INSUFFICIENT_STORAGE -> "phone storage is full"
    VideoRecordEvent.Finalize.ERROR_SOURCE_INACTIVE -> "the camera was interrupted"
    VideoRecordEvent.Finalize.ERROR_DURATION_LIMIT_REACHED, VideoRecordEvent.Finalize.ERROR_FILE_SIZE_LIMIT_REACHED -> "size limit reached"
    VideoRecordEvent.Finalize.ERROR_NO_VALID_DATA -> "nothing was recorded"
    else -> "recording error $code"
}

@Composable
private fun CameraNotice(text: String, action: String?, onAction: () -> Unit) {
    Column(Modifier.fillMaxSize().padding(32.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) {
        Text(text, color = Color.White, style = MaterialTheme.typography.titleMedium, textAlign = TextAlign.Center)
        if (action != null) {
            Spacer(Modifier.height(16.dp))
            ActionButton(onClick = onAction, modifier = Modifier.defaultMinSize(minHeight = 56.dp)) { Text(action) }
        }
    }
}

/** Round icon button on a dark scrim, readable over any scene. */
@Composable
private fun ScrimIconBtn(icon: Int, desc: String, onClick: () -> Unit) {
    androidx.compose.material3.IconButton(onClick = onClick, modifier = Modifier.size(Touch).background(Scrim, CircleShape)) {
        androidx.compose.material3.Icon(androidx.compose.ui.res.painterResource(icon), desc, tint = Color.White)
    }
}

@Composable
private fun FolderChip(chip: UiLogic.ChipUi, enabled: Boolean, onClick: () -> Unit) {
    Row(
        // Long names (and large font sizes) shorten the name, never push the arrow out of the chip.
        Modifier.widthIn(max = 320.dp).defaultMinSize(minHeight = Touch).clip(RoundedCornerShape(50)).background(Scrim)
            .clickable(enabled = enabled, role = Role.Button, onClick = onClick)
            .padding(start = 14.dp, end = 8.dp).semantics { contentDescription = "Next shot goes to ${chip.text}. Tap to change" },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Ico(if (chip.noLocation) R.drawable.ic_location_off else R.drawable.ic_folder, null, tint = Color.White, size = 20)
        Spacer(Modifier.width(6.dp))
        Text(chip.text, color = Color.White, style = MaterialTheme.typography.titleSmall, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
        if (enabled) Ico(R.drawable.ic_arrow_drop_down, null, tint = Color.White) else Spacer(Modifier.width(8.dp))
    }
}

@Composable
private fun ZoomChip(label: String, selected: Boolean, onClick: () -> Unit) {
    Box(
        Modifier.size(Touch).clip(CircleShape).background(if (selected) Color.White else Color.Transparent)
            .clickable(role = Role.Button, onClick = onClick).semantics { this.selected = selected; contentDescription = "Zoom $label" },
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = if (selected) Color.Black else Color.White, style = MaterialTheme.typography.labelLarge, fontWeight = if (selected) FontWeight.Bold else FontWeight.Normal)
    }
}

@Composable
private fun ModeChip(label: String, selected: Boolean, onClick: () -> Unit) {
    Box(
        Modifier.defaultMinSize(minHeight = Touch).clip(RoundedCornerShape(50)).background(if (selected) Color.White else Scrim)
            .clickable(role = Role.Tab, onClick = onClick).padding(horizontal = 20.dp).semantics { this.selected = selected },
        contentAlignment = Alignment.Center,
    ) {
        Text(label.uppercase(), color = if (selected) Color.Black else Color.White, style = MaterialTheme.typography.labelLarge, fontWeight = if (selected) FontWeight.Bold else FontWeight.Normal)
    }
}

@Composable
private fun LastShot(latest: uniffi.tugboat_ffi.ItemInfo?, onClick: () -> Unit) {
    // The dark halo keeps the white border visible over a white scene.
    Box(
        Modifier.background(Scrim, RoundedCornerShape(14.dp)).padding(2.dp).size(56.dp).clip(RoundedCornerShape(12.dp))
            .border(BorderStroke(2.dp, Color.White), RoundedCornerShape(12.dp)).background(Scrim)
            .clickable(role = Role.Button, onClick = onClick).described("Open gallery"),
        contentAlignment = Alignment.Center,
    ) {
        if (latest?.thumbPath != null) ThumbImage(latest.thumbPath, null, Modifier.fillMaxSize()) else Ico(R.drawable.ic_photo_library, null, tint = Color.White)
    }
}

/**
 * 80 dp shutter on a 75% black disc (worst case, a white scene: #404040): buoy-orange ring, 4.5:1 there; photo = white
 * disc, video = red disc, recording = red square. Disabled = grey, not faded white (faded white vanishes on a bright
 * scene). Words for screen readers.
 */
@Composable
private fun Shutter(video: Boolean, recording: Boolean, enabled: Boolean, onClick: () -> Unit) {
    val desc = if (!video) "Take photo" else if (recording) "Stop recording" else "Start recording"
    val ring = if (enabled) ShutterRing else ShutterOff
    Box(Modifier.size(88.dp).background(ShutterDisc, CircleShape), contentAlignment = Alignment.Center) {
        Box(
            Modifier.size(80.dp).clip(CircleShape).border(4.dp, ring, CircleShape)
                .clickable(enabled = enabled, role = Role.Button, onClick = onClick).semantics { contentDescription = desc },
            contentAlignment = Alignment.Center,
        ) {
            val fill = if (!enabled) ShutterOff else if (video) Color(0xFFE5483B) else Color.White
            Box(
                Modifier.size(if (recording) 30.dp else 62.dp)
                    .clip(if (recording) RoundedCornerShape(6.dp) else CircleShape)
                    .background(fill),
            )
        }
    }
}

/** Disabled shutter on the scrim: light grey, 7:1 on the worst disc (white scene), unlike white at 40% alpha. */
private val ShutterOff = Color(0xFFC0C0C0)
