package app.localtugboat

import android.os.Build
import androidx.activity.compose.LocalActivity
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import java.util.concurrent.Executors

/** First run: permissions, then scan the PC's QR code, then the code check. Every state has words and one way forward. */
@Composable
fun Onboarding(vm: SnapViewModel, ui: UiState, actions: ShellActions) {
    val pairing by vm.pairing.collectAsStateWithLifecycle()
    val hardDenied by vm.hardDenied.collectAsStateWithLifecycle()
    val continued by vm.permsContinued.collectAsStateWithLifecycle()
    val rows = ui.perms?.let { UiLogic.permRows(it, hardDenied, Build.VERSION.SDK_INT) }
    val step = UiLogic.onboardingStep(rows, continued, pairing)

    when (step) {
        OnboardingStep.PERMISSIONS -> PermissionStep(rows, vm::continuePermissions, actions)
        OnboardingStep.DONE -> DoneStep((pairing as PairUi.Done).peer)
        else -> PairSteps(vm, ui, step, pairing, hardDenied, actions)
    }
}

/**
 * Link another PC from inside the app: the same scan, code and failure steps as the first run. A success returns to
 * Settings; leaving the screen stops a pairing in progress.
 */
@Composable
fun AddDeviceScreen(vm: SnapViewModel, ui: UiState, actions: ShellActions) {
    val pairing by vm.pairing.collectAsStateWithLifecycle()
    val hardDenied by vm.hardDenied.collectAsStateWithLifecycle()
    val rows = ui.perms?.let { UiLogic.permRows(it, hardDenied, Build.VERSION.SDK_INT) }
    // A rotation or a theme change recreates the activity: the pairing in progress carries on in the view model.
    val activity = LocalActivity.current
    DisposableEffect(Unit) {
        onDispose {
            if (activity?.isChangingConfigurations != true) {
                vm.cancelPairing()
                vm.resetPairing()
            }
        }
    }
    LaunchedEffect(pairing) {
        if (pairing is PairUi.Done) {
            vm.resetPairing()
            vm.nav.back()
        }
    }
    // Permissions were asked on the first run: only a required one that was turned off since is shown again.
    when (val step = UiLogic.onboardingStep(rows, true, pairing)) {
        OnboardingStep.PERMISSIONS -> PermissionStep(rows, vm::continuePermissions, actions)
        OnboardingStep.DONE -> DoneStep((pairing as PairUi.Done).peer)
        else -> PairSteps(vm, ui, step, pairing, hardDenied, actions, onCancel = { vm.nav.back() })
    }
}

/** Scan, connecting, code check and failure: shared by the first run and Link a PC. [onCancel] adds a Cancel button to the scanner. */
@Composable
private fun PairSteps(vm: SnapViewModel, ui: UiState, step: OnboardingStep, pairing: PairUi, hardDenied: Set<PermKind>, actions: ShellActions, onCancel: (() -> Unit)? = null) {
    // "LOCAL NETWORK BLOCKED" is stale the moment the permission is on: go back to the scanner.
    val failedMessage = (pairing as? PairUi.Failed)?.message
    val netOn = ui.perms?.localNetwork == true
    LaunchedEffect(failedMessage, netOn) {
        if (failedMessage != null && netOn && UiLogic.failAction(failedMessage) == FailAction.ALLOW) vm.resetPairing()
    }

    when (step) {
        OnboardingStep.CONNECTING -> Centered {
            CircularProgressIndicator(Modifier.size(48.dp))
            Spacer(Modifier.height(20.dp))
            Text("Connecting to your PC", style = MaterialTheme.typography.titleMedium)
        }
        OnboardingStep.CODE -> CodeStep((pairing as PairUi.Code), vm::cancelPairing)
        OnboardingStep.FAILED -> FailedStep((pairing as PairUi.Failed).message, vm::resetPairing, actions, hardDenied)
        else -> ScanStep(vm, onCancel)
    }
}

@Composable
private fun DoneStep(peer: String) {
    Centered {
        Ico(R.drawable.ic_check, null, tint = MaterialTheme.colorScheme.primary, size = 48)
        Spacer(Modifier.height(12.dp))
        Text("Linked to $peer", style = MaterialTheme.typography.titleMedium)
    }
}

@Composable
private fun Centered(content: @Composable () -> Unit) {
    Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().padding(24.dp), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.Center) { content() }
}

@Composable
private fun PermissionStep(rows: List<PermRow>?, onContinue: () -> Unit, actions: ShellActions) {
    Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().padding(horizontal = 24.dp)) {
        Column(Modifier.weight(1f).verticalScroll(rememberScrollState())) {
            Spacer(Modifier.height(32.dp))
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                TugboatArt(64.dp)
                Text("Tugboat", style = MaterialTheme.typography.headlineMedium)
            }
            Text("Photos you take here show up on your PC.", style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.height(16.dp))
            if (rows == null) {
                CircularProgressIndicator(Modifier.padding(24.dp).size(32.dp))
            } else {
                rows.forEach { r ->
                    PermissionRow(r, onAllow = { actions.request(r.kind) }, onSettings = actions.openAppSettings)
                }
            }
        }
        val ready = rows != null && rows.none { it.required && it.status != PermStatus.GRANTED }
        if (!ready) {
            Text("Allow camera and local network to continue.", style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(bottom = 8.dp))
        }
        ActionButton(onClick = onContinue, enabled = ready, modifier = Modifier.fillMaxWidth().defaultMinSize(minHeight = 56.dp)) { Text("Continue") }
        Spacer(Modifier.height(16.dp))
    }
}

@Composable
private fun ScanStep(vm: SnapViewModel, onCancel: (() -> Unit)? = null) {
    val scrimText = Modifier.background(Scrim, MaterialTheme.shapes.medium).padding(horizontal = 14.dp, vertical = 8.dp)
    LightSystemIcons()
    Box(Modifier.fillMaxSize().background(Color.Black)) {
        QrScanner(onPayload = vm::pair, onError = { vm.say(it) }, modifier = Modifier.fillMaxSize())
        Column(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding().imePadding().padding(16.dp), horizontalAlignment = Alignment.CenterHorizontally) {
            Text("Scan the QR code on your PC", style = MaterialTheme.typography.titleMedium, color = Color.White, modifier = scrimText)
            Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                // Full outline on all four sides: a framing aid, not an accent stripe.
                Box(Modifier.size(240.dp).clip(MaterialTheme.shapes.large).border(BorderStroke(2.dp, Color.White), MaterialTheme.shapes.large))
            }
            Text("On your PC click Link phone.", style = MaterialTheme.typography.bodyMedium, color = Color.White, modifier = scrimText)
            if (BuildConfig.DEBUG) DebugPaste(vm)
            if (onCancel != null) TextButton(onClick = onCancel, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Cancel", color = Color.White) }
        }
    }
}

/** Emulator tests: the emulator camera cannot scan a QR code shown on the host screen. Debug builds only. */
@Composable
private fun DebugPaste(vm: SnapViewModel) {
    var text by remember { mutableStateOf("") }
    Row(Modifier.fillMaxWidth().padding(top = 12.dp).background(Scrim, MaterialTheme.shapes.medium).padding(8.dp), verticalAlignment = Alignment.CenterVertically) {
        OutlinedTextField(value = text, onValueChange = { text = it }, label = { Text("Paste pairing code") }, singleLine = true, modifier = Modifier.weight(1f))
        Spacer(Modifier.size(8.dp))
        ActionButton(onClick = { vm.pair(text) }, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Pair") }
    }
}

@Composable
private fun CodeStep(p: PairUi.Code, onCancel: () -> Unit) {
    Centered {
        Text("Check your PC shows", style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.height(12.dp))
        Text(
            UiLogic.formatCode(p.code), fontSize = 56.sp, fontWeight = FontWeight.Bold, fontFamily = FontFamily.Monospace,
            textAlign = TextAlign.Center, modifier = Modifier.described("Pairing code ${p.code.filter { it.isDigit() }.toList().joinToString(" ")}"),
        )
        Spacer(Modifier.height(12.dp))
        Text("Then click Link on ${p.peer}.", style = MaterialTheme.typography.bodyLarge, textAlign = TextAlign.Center)
        Spacer(Modifier.height(24.dp))
        CircularProgressIndicator(Modifier.size(28.dp))
        Spacer(Modifier.height(24.dp))
        OutlinedButton(onClick = onCancel, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Cancel") }
    }
}

@Composable
private fun FailedStep(message: String, onRetry: () -> Unit, actions: ShellActions, hardDenied: Set<PermKind>) {
    val cs = MaterialTheme.colorScheme
    Centered {
        Ico(R.drawable.ic_warning, null, tint = cs.error, size = 40)
        Spacer(Modifier.height(12.dp))
        Text(message, style = MaterialTheme.typography.titleMedium, color = cs.error, textAlign = TextAlign.Center)
        Spacer(Modifier.height(24.dp))
        if (UiLogic.failAction(message) == FailAction.ALLOW) {
            val settings = PermKind.LOCAL_NETWORK in hardDenied
            ActionButton(onClick = { if (settings) actions.openAppSettings() else actions.request(PermKind.LOCAL_NETWORK) }, modifier = Modifier.defaultMinSize(minHeight = 56.dp)) {
                Text(if (settings) "Open settings" else "Allow local network")
            }
            TextButton(onClick = onRetry, modifier = Modifier.defaultMinSize(minHeight = Touch)) { Text("Scan again") }
        } else {
            ActionButton(onClick = onRetry, modifier = Modifier.defaultMinSize(minHeight = 56.dp)) { Text("Scan again") }
        }
    }
}

/**
 * Live camera that reads QR codes (bundled ML Kit model). Ignores QR codes that are not Tugboat pairing
 * text. Bound to the lifecycle: the camera is released when the screen goes away.
 */
@Composable
fun QrScanner(onPayload: (String) -> Unit, onError: (String) -> Unit, modifier: Modifier = Modifier) {
    val ctx = LocalContext.current
    val owner = LocalLifecycleOwner.current
    val view = remember { PreviewView(ctx).apply { scaleType = PreviewView.ScaleType.FILL_CENTER } }
    val payload by rememberUpdatedState(onPayload)
    val error by rememberUpdatedState(onError)

    DisposableEffect(owner) {
        val analyzer = QrAnalyzer({ payload(it) }, { error(it) })
        val executor = Executors.newSingleThreadExecutor()
        val future = ProcessCameraProvider.getInstance(ctx)
        val main = ContextCompat.getMainExecutor(ctx)
        var provider: ProcessCameraProvider? = null
        // Set first thing on dispose: a provider future that completes after the scanner left must not bind the camera.
        var disposed = false
        future.addListener({
            if (disposed) return@addListener
            try {
                val p = future.get()
                provider = p
                val preview = Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider }
                val analysis = ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
                    .also { it.setAnalyzer(executor, analyzer) }
                p.unbindAll()
                p.bindToLifecycle(owner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
            } catch (e: Exception) {
                error("Camera: ${e.message ?: e.javaClass.simpleName}")
            }
        }, main)
        onDispose {
            disposed = true
            provider?.unbindAll()
            executor.shutdown()
            analyzer.close()
        }
    }
    AndroidView({ view }, modifier)
}
