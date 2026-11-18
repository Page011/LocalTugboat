package app.localtugboat

import android.app.Application
import android.content.Intent
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.tugboat_ffi.ActivityInfo
import uniffi.tugboat_ffi.FfiEvent
import uniffi.tugboat_ffi.FolderInfo
import uniffi.tugboat_ffi.ItemInfo
import uniffi.tugboat_ffi.ItemPage
import uniffi.tugboat_ffi.PairedPc
import uniffi.tugboat_ffi.Settings
import uniffi.tugboat_ffi.SyncStatus
import uniffi.tugboat_ffi.inspectPairingPayload

/** Pairing screen state. */
sealed interface PairUi {
    data object Idle : PairUi
    /** Dialing the PC; the code is not known yet. */
    data object Dialing : PairUi
    /** "Check your PC shows 482 913": waiting for the PC owner to click Pair. */
    data class Code(val code: String, val peer: String) : PairUi
    data class Done(val peer: String) : PairUi
    /** One readable line; uppercase key word for blockers. */
    data class Failed(val message: String) : PairUi
}

/** A deep link waiting behind the "Pair with this PC?" dialog. */
data class DeepLink(val payload: String, val pcName: String, val addresses: List<String>)

/** Everything the app-level banners and settings need. Null fields = not loaded yet. */
data class UiState(
    val loaded: Boolean = false,
    /** Blocker: the core could not open (KEY UNREADABLE, newer database...). Nothing else works. */
    val openError: String? = null,
    val paired: PairedPc? = null,
    val settings: Settings? = null,
    val status: SyncStatus? = null,
    val perms: PermState? = null,
    /** Start of a "Tugboat was stopped" gap to show a banner for, if any. */
    val stoppedGapFromMs: Long? = null,
    val freeBytes: Long? = null,
    /** Free space is under the storage guard setting. */
    val storageLow: Boolean = false,
    /** Newest background errors, newest last. */
    val errors: List<String> = emptyList(),
)

/**
 * What the screens use. Every method that touches the core runs on Dispatchers.IO; failures reach the
 * user through [messages] and are always in the log. Screens never call PhoneCore directly.
 */
class SnapViewModel(app: Application) : AndroidViewModel(app) {
    private val ctx get() = getApplication<Application>()
    private val core get() = ctx.core

    private data class Meta(
        val openError: String? = null, val paired: PairedPc? = null, val settings: Settings? = null,
        val perms: PermState? = null, val gapFrom: Long? = null, val freeBytes: Long? = null, val loaded: Boolean = false,
    )

    private val meta = MutableStateFlow(Meta())

    val ui: StateFlow<UiState> = combine(meta, core.status, core.errors) { m, st, errs ->
        UiState(
            loaded = m.loaded, openError = m.openError, paired = m.paired, settings = m.settings, status = st, perms = m.perms,
            stoppedGapFromMs = m.gapFrom, freeBytes = m.freeBytes,
            storageLow = m.freeBytes != null && m.settings != null && m.freeBytes.toULong() < m.settings.storageGuardBytes,
            errors = errs,
        )
    }.stateIn(viewModelScope, SharingStarted.Eagerly, UiState())

    private val _pairing = MutableStateFlow<PairUi>(PairUi.Idle)
    val pairing: StateFlow<PairUi> = _pairing

    private val _deepLink = MutableStateFlow<DeepLink?>(null)
    val deepLink: StateFlow<DeepLink?> = _deepLink

    private val _folders = MutableStateFlow<List<FolderInfo>>(emptyList())
    val folders: StateFlow<List<FolderInfo>> = _folders

    /** Short results and failures of user actions, for a snackbar or toast. */
    private val _messages = MutableSharedFlow<String>(extraBufferCapacity = 16, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    val messages: SharedFlow<String> = _messages

    /** Ids of items just moved to the Bin by the user: the Shell offers "Moved to Bin [Undo]" for each. */
    private val _binned = MutableSharedFlow<String>(extraBufferCapacity = 8, onBufferOverflow = BufferOverflow.DROP_OLDEST)
    val binned: SharedFlow<String> = _binned

    /** Where the user is inside the paired app. Lives here so a rotation does not send the user back to the camera. */
    val nav = NavStack()

    /** The newest item outside the Bin: the last-shot thumbnail on the camera. Null = nothing yet. */
    private val _latest = MutableStateFlow<ItemInfo?>(null)
    val latest: StateFlow<ItemInfo?> = _latest

    /** Permission kinds the system will not show a dialog for any more: their Allow button opens app settings. */
    private val _hardDenied = MutableStateFlow<Set<PermKind>>(emptySet())
    val hardDenied: StateFlow<Set<PermKind>> = _hardDenied

    /** The user pressed Continue on the permission list this run. */
    private val _permsContinued = MutableStateFlow(false)
    val permsContinued: StateFlow<Boolean> = _permsContinued

    /** One refresh at a time, in call order: an older read must never publish after a newer one (stale paired/unpaired). Declared before init: init calls refresh(). */
    @OptIn(ExperimentalCoroutinesApi::class)
    private val refreshDispatcher = Dispatchers.IO.limitedParallelism(1)

    /** Gallery lists kept per folder (see [itemsModel]). Declared before init: the event collector's unpair reset clears it. */
    private val models = java.util.concurrent.ConcurrentHashMap<Pair<String?, Boolean>, PagedModel<ItemInfo>>()

    /** Core events for screens that show lists: re-read what is on screen when one arrives. */
    val events: SharedFlow<FfiEvent> get() = core.events

    init {
        viewModelScope.launch(Dispatchers.IO) {
            core.events.collect { e ->
                when (e) {
                    is FfiEvent.PairingCode -> _pairing.value = PairUi.Code(e.code, e.peerName)
                    is FfiEvent.Unpaired -> { resetAfterUnpair(); refresh() }
                    is FfiEvent.PairingCompleted, is FfiEvent.SettingsChanged, FfiEvent.Resync -> refresh()
                    else -> {}
                }
                when (e) {
                    is FfiEvent.FoldersChanged, is FfiEvent.ItemsChanged, FfiEvent.Resync -> { refreshFolders(); refreshLatest() }
                    else -> {}
                }
            }
        }
        refresh()
        refreshFolders()
        refreshLatest()
    }

    // ---- lifecycle hooks the activity calls ----

    /** App came to the front: network on, service on if wanted, everything re-read. */
    fun onForeground() {
        core.hold("ui")
        viewModelScope.launch(Dispatchers.IO) {
            ServiceGate.startIfWanted(ctx, "app opened")
            core.phoneIfOpen()?.reconnectNow()
            // Shots whose hand-off failed or whose process died mid hand-off: take them now.
            if (core.phoneIfOpen() != null && Capture.recover(ctx) > 0) {
                refreshFolders()
                refreshLatest()
            }
            refresh()
        }
        refreshFolders()
        refreshLatest()
    }

    fun onBackground() {
        core.release("ui")
    }

    /**
     * A permission dialog closed. [requested] = kinds asked, [noRationale] = kinds the system will not explain
     * (after a denial that means "don't ask again"). Updates which rows must open app settings instead.
     */
    fun onPermissionResult(requested: Set<PermKind>, noRationale: Set<PermKind>) {
        viewModelScope.launch(Dispatchers.IO) {
            _hardDenied.value = UiLogic.hardDenied(_hardDenied.value, requested, noRationale, Perms.snapshot(ctx))
            onPermissionsChanged()
        }
    }

    fun continuePermissions() {
        _permsContinued.value = true
    }

    /** Call after any permission dialog or after returning from system settings. */
    fun onPermissionsChanged() {
        val wasBlocked = meta.value.perms?.localNetwork == false
        refresh()
        if (wasBlocked && Perms.localNetworkAllowed(ctx)) core.restartLink() // mDNS can switch on now
    }

    override fun onCleared() {
        core.phoneIfOpen()?.cancelPairing()
    }

    // ---- state ----

    /** Re-read the small facts behind the banners. Safe to call often. */
    fun refresh() {
        viewModelScope.launch(refreshDispatcher) {
            val perms = Perms.snapshot(ctx)
            try {
                val p = core.phone()
                val paired = p.pairedPc()
                val settings = p.settings()
                val gap = if (paired != null && settings.stayConnected) Heartbeat.lastGap(ctx, System.currentTimeMillis())?.fromMs else null
                meta.value = Meta(null, paired, settings, perms, gap, p.freeSpaceBytes().toLong(), true)
            } catch (e: Exception) {
                core.reportError("open: ${e.message}")
                meta.value = meta.value.copy(openError = e.message ?: "Core did not open", perms = perms, loaded = true)
            }
        }
    }

    fun refreshLatest() {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                _latest.value = core.phone().items(null, false, null, 1u).rows.firstOrNull()
            } catch (e: Exception) {
                fail("Latest photo", e)
            }
        }
    }

    fun refreshFolders() {
        viewModelScope.launch(Dispatchers.IO) {
            try {
                _folders.value = core.phone().folders()
            } catch (e: Exception) {
                fail("Folders", e)
            }
        }
    }

    /** A short result for the user (snackbar) from a screen. */
    fun say(text: String) {
        _messages.tryEmit(text)
    }

    /** A failure a screen caught itself: logged and shown, same as the model's own. */
    fun reportFailure(what: String, e: Exception) = fail(what, e)

    private fun fail(what: String, e: Exception) {
        val text = "$what: ${e.message ?: e.javaClass.simpleName}"
        core.reportError(text)
        _messages.tryEmit(text)
    }

    private fun act(what: String, block: () -> Unit): Job =
        viewModelScope.launch(Dispatchers.IO) {
            try {
                block()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                fail(what, e)
            }
        }

    // ---- pairing ----

    /** From the scanner or the debug paste field. Text that is not a pairing payload is refused with a message. */
    fun pair(text: String) {
        val payload = PairingText.candidate(text)
        if (payload == null) {
            _pairing.value = PairUi.Failed("That is not a Tugboat code")
            return
        }
        if (_pairing.value is PairUi.Dialing || _pairing.value is PairUi.Code) return
        _pairing.value = PairUi.Dialing
        viewModelScope.launch(Dispatchers.IO) {
            // The code's expiry (the PC's stamp, compared with this phone's clock) is read before dialing: a failure
            // after it means "expired", whatever words the PC's newer window used to refuse the old secret.
            val expiresAt = try {
                inspectPairingPayload(payload).expiresAtMs
            } catch (e: Exception) {
                null
            }
            // The hold is inside the try: if this job is cancelled while waiting for it, the release in finally still runs.
            try {
                core.hold("pairing").join() // pairing needs the endpoint up
                val pc = core.phone().pairWith(payload)
                _pairing.value = PairUi.Done(pc.name)
                // The service takes over the network hold before "pairing" lets go: no gap in between.
                ServiceGate.startIfWanted(ctx, "paired")
                refresh()
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                val line = Texts.pairFailure(e, Perms.localNetworkAllowed(ctx), codeExpired = expiresAt != null && System.currentTimeMillis() >= expiresAt)
                // The onboarding screen shows this failure; a banner after a later success would be stale, so log only.
                KLog.log(ctx, "pairing failed: $line")
                _pairing.value = PairUi.Failed(line)
            } finally {
                core.release("pairing")
            }
        }
    }

    fun cancelPairing() {
        core.phoneIfOpen()?.cancelPairing()
    }

    fun resetPairing() {
        if (_pairing.value !is PairUi.Dialing && _pairing.value !is PairUi.Code) _pairing.value = PairUi.Idle
    }

    /**
     * A deep link (intent data). Never reads extras. Shows the confirmation first: see [confirmDeepLink].
     * "Already paired" is asked of the core, not of [meta]: a link that starts the app arrives before [refresh]
     * has loaded anything, and an unloaded state must not read as "not paired".
     */
    fun onDeepLink(text: String?) {
        val payload = PairingText.candidate(text)
        viewModelScope.launch(Dispatchers.IO) {
            try {
                if (core.phone().pairedPc() != null) {
                    _messages.tryEmit("Already paired. Unpair in Settings first")
                    return@launch
                }
                if (payload == null) {
                    _messages.tryEmit("That link is not a Tugboat code")
                    return@launch
                }
                val pv = inspectPairingPayload(payload)
                _deepLink.value = DeepLink(payload, pv.pcName, pv.addresses)
            } catch (e: Exception) {
                fail("Pairing link", e)
            }
        }
    }

    /** The dialog's Pair button. Asks the core again: the phone may have paired while the dialog was open. */
    fun confirmDeepLink() {
        val d = _deepLink.value ?: return
        _deepLink.value = null
        viewModelScope.launch(Dispatchers.IO) {
            try {
                if (core.phone().pairedPc() != null) {
                    _messages.tryEmit("Already paired. Unpair in Settings first")
                    return@launch
                }
                pair(d.payload)
            } catch (e: Exception) {
                fail("Pairing link", e)
            }
        }
    }

    fun dismissDeepLink() {
        _deepLink.value = null
    }

    // ---- gallery ----

    /**
     * The rows of one gallery list ([folderId] null = all folders), kept here so coming back from the viewer
     * shows the same rows at once and the grid keeps its scroll position. The screen reloads it on entry.
     */
    fun itemsModel(folderId: String?, bin: Boolean): PagedModel<ItemInfo> =
        models.getOrPut(folderId to bin) { PagedModel { cursor, n -> items(folderId, bin, cursor, n.toUInt()).let { PageOf(it.rows, it.next) } } }

    suspend fun items(folderId: String?, bin: Boolean, after: String?, limit: UInt = 50u): ItemPage =
        withContext(Dispatchers.IO) { core.phone().items(folderId, bin, after, limit) }

    suspend fun item(id: String): ItemInfo? = withContext(Dispatchers.IO) { core.phone().item(id) }

    suspend fun activity(before: Long?, limit: UInt = 50u): List<ActivityInfo> =
        withContext(Dispatchers.IO) { core.phone().activity(before, limit) }

    /** These return the running job: a screen joins it, then re-reads its list, so it never shows the old state. */
    fun keep(id: String, keep: Boolean): Job = act("Keep") { core.phone().setPinned(id, keep) }
    fun move(id: String, folderId: String): Job = act("Move") { core.phone().moveItem(id, folderId) }
    fun delete(id: String): Job = act("Delete") {
        core.phone().deleteItem(id)
        _binned.tryEmit(id) // only after the core took it: a failed delete shows its error, never "Moved to Bin"
    }
    fun restore(id: String): Job = act("Restore") { core.phone().restoreItem(id) }

    /** The share sheet intent for an item, or null (with a message) when the file is not on this phone. */
    suspend fun shareIntent(item: ItemInfo): Intent? = withContext(Dispatchers.IO) {
        try {
            ItemExport.shareIntent(ctx, item)
        } catch (e: Exception) {
            fail("Share", e)
            null
        }
    }

    /** ACTION_VIEW intent for a held file (content URI, read grant), or null (with a message) when it cannot be opened. */
    suspend fun openIntent(item: ItemInfo): Intent? = withContext(Dispatchers.IO) {
        try {
            ItemFileProvider.openIntent(ctx, item)
        } catch (e: Exception) {
            fail("Open", e)
            null
        }
    }

    fun saveToGallery(item: ItemInfo) = act("Save to phone") {
        ItemExport.saveToGallery(ctx, item)
        _messages.tryEmit("Saved to phone gallery")
    }

    // ---- settings and maintenance ----

    /** Settings are saved as one whole record: read, change one field, save must not interleave with another setter. */
    private val settingsLock = Any()

    fun setStayConnected(on: Boolean) = act("Settings") {
        synchronized(settingsLock) {
            val p = core.phone()
            p.saveSettings(p.settings().copy(stayConnected = on))
            if (on) ServiceGate.startIfWanted(ctx, "setting on") else SnapService.stop(ctx)
        }
        refresh()
    }

    fun setBinDays(days: UInt) = act("Settings") {
        synchronized(settingsLock) {
            val p = core.phone()
            p.saveSettings(p.settings().copy(binDays = days))
        }
        refresh()
    }

    fun unpair() = act("Unpair") {
        core.phone().unpair()
        SnapService.stop(ctx)
        resetAfterUnpair()
        refresh()
    }

    /** The PC unpaired too (event) or the user did: forget everything that belonged to the pairing. */
    private fun resetAfterUnpair() {
        _pairing.update { UiLogic.pairingAfterUnpair(it) }
        _permsContinued.value = false
        _latest.value = null
        models.clear()
        nav.home()
    }

    /** The share sheet intent for the exported log, or null (with a message) if it could not be built. */
    suspend fun exportLog(): Intent? = withContext(Dispatchers.IO) {
        try {
            LogExport.intent(ctx)
        } catch (e: Exception) {
            fail("Export log", e)
            null
        }
    }

    /** The user saw the "stopped" banner or tapped Fix. */
    fun acknowledgeGap() {
        Heartbeat.acknowledge(ctx)
        refresh()
    }

    fun clearErrors() = core.clearErrors()
}
