package app.localtugboat

import androidx.activity.compose.BackHandler
import androidx.compose.ui.Alignment
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import java.time.ZoneId

/** What the screens ask the activity to do: permission dialogs, system screens, share sheets. */
class ShellActions(
    /** Ask for one permission row. The activity opens app settings itself where no dialog can appear. */
    val request: (PermKind) -> Unit,
    val openAppSettings: () -> Unit,
    val exportLog: () -> Unit,
    val fixBattery: () -> Unit,
)

/** Top level: loading, fatal, first run (onboarding) or the paired app. Messages and the pairing-link dialog sit on top of all. */
@Composable
fun Shell(vm: SnapViewModel, actions: ShellActions) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    val deepLink by vm.deepLink.collectAsStateWithLifecycle()
    val snack = remember { SnackbarHostState() }
    LaunchedEffect(Unit) { vm.messages.collect { snack.showSnackbar(it) } }
    // A delete is one tap away from the viewer's other buttons: say where it went and let the user take it back.
    LaunchedEffect(Unit) {
        vm.binned.collect { id ->
            if (snack.showSnackbar("Moved to Bin", actionLabel = "Undo", duration = SnackbarDuration.Long) == SnackbarResult.ActionPerformed) vm.restore(id)
        }
    }

    val root = UiLogic.root(ui)
    val onCamera = root == Root.MAIN && vm.nav.top == Route.Camera
    // Surface, not a plain background: it also sets the default text colour, so dark mode text is not black.
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.surface) {
        Box(Modifier.fillMaxSize()) {
        when (root) {
            Root.LOADING -> Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) { CircularProgressIndicator() }
            Root.FATAL -> Box(Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()) {
                Notice(ui.openError ?: "", Modifier.align(Alignment.Center), "Export log", actions.exportLog, blocker = true)
            }
            Root.ONBOARDING -> Onboarding(vm, ui, actions)
            Root.MAIN -> MainNav(vm, ui, actions)
        }
        // Over the camera it must clear the shutter row.
        SnackbarHost(snack, Modifier.align(Alignment.BottomCenter).navigationBarsPadding().padding(bottom = if (onCamera) 232.dp else 8.dp))
        }
    }

    deepLink?.let { d ->
        AlertDialog(
            onDismissRequest = vm::dismissDeepLink,
            title = { Text("Link this PC?") },
            text = { Text("${d.pcName}\n${d.addresses.joinToString()}") },
            confirmButton = { TextButton(onClick = vm::confirmDeepLink) { Text("Link") } },
            dismissButton = { TextButton(onClick = vm::dismissDeepLink) { Text("Cancel") } },
        )
    }
}

@Composable
private fun MainNav(vm: SnapViewModel, ui: UiState, actions: ShellActions) {
    val nav = vm.nav
    val hardDenied by vm.hardDenied.collectAsStateWithLifecycle()
    // Each screen keeps its scroll position and typed state while another is on top.
    val holder = rememberSaveableStateHolder()
    BackHandler(enabled = nav.routes.size > 1) { nav.back() }

    // A viewer that left the stack must not keep its saved pager page: opening the same photo again starts on that photo.
    val seen = remember { mutableSetOf<Route>() }
    val routesNow = nav.routes.toList()
    LaunchedEffect(routesNow) {
        seen += routesNow
        for (key in UiLogic.droppedViewerKeys(seen, routesNow)) holder.removeState(key)
        seen.retainAll(routesNow.toSet())
    }

    val banners = remember(ui) { UiLogic.banners(ui, ZoneId.systemDefault()) }
    val onBanner: (UiLogic.BannerUi) -> Unit = { b ->
        fun ask(k: PermKind) = if (k in hardDenied) actions.openAppSettings() else actions.request(k)
        when (b.id) {
            UiLogic.BannerId.LOCAL_NETWORK -> ask(PermKind.LOCAL_NETWORK)
            UiLogic.BannerId.NOTIFICATIONS -> ask(PermKind.NOTIFICATIONS)
            UiLogic.BannerId.STOPPED -> actions.fixBattery()
            UiLogic.BannerId.ERROR -> vm.clearErrors()
            UiLogic.BannerId.OPEN_ERROR, UiLogic.BannerId.STORAGE, UiLogic.BannerId.SYNC -> {}
        }
    }

    val route = nav.top
    holder.SaveableStateProvider(route.toString()) {
        when (route) {
            Route.Camera -> CameraScreen(vm, ui, banners, onBanner, actions)
            is Route.Gallery -> GalleryScreen(vm, ui, banners, onBanner, route)
            Route.Bin -> BinScreen(vm, ui)
            Route.Activity -> ActivityScreen(vm)
            Route.Settings -> SettingsScreen(vm, ui, banners, onBanner, actions)
            Route.AddDevice -> AddDeviceScreen(vm, ui, actions)
            is Route.Viewer -> ViewerScreen(vm, ui, route)
        }
    }
}
