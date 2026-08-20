package dev.bifrost.app

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Chat
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.FileProvider
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch
import java.io.File
import java.util.Locale

/**
 * The Bifrost Android client.
 *
 * The phone is a genuine peer here, not a browser controlling a laptop: the
 * Rust engine runs in this process, opens its own sockets, beacons on the LAN
 * and is discovered by every other device. It keeps working with every laptop
 * asleep.
 */
class MainActivity : ComponentActivity() {

    private lateinit var repository: BifrostRepository

    /**
     * Android 13+ refuses to let an app see devices on the local network
     * without this. Discovery silently finds nothing if it is missing, which
     * looks exactly like a broken network — so ask up front.
     */
    private val requestPermissions =
        registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { }

    /** Picking a file to send. */
    private var pendingPeer: String? = null
    private val pickFile =
        registerForActivityResult(ActivityResultContracts.GetContent()) { uri ->
            val peer = pendingPeer ?: return@registerForActivityResult
            pendingPeer = null
            uri?.let { sendFromUri(peer, it) }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        repository = BifrostRepository(applicationContext)

        val wanted = buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                add(Manifest.permission.NEARBY_WIFI_DEVICES)
                add(Manifest.permission.POST_NOTIFICATIONS)
            }
            add(Manifest.permission.RECORD_AUDIO)
        }
        requestPermissions.launch(wanted.toTypedArray())

        lifecycleScope.launch { repository.start(lifecycleScope) }

        setContent {
            BifrostTheme {
                val state by repository.state.collectAsState()
                BifrostApp(
                    state = state,
                    repository = repository,
                    onPickFile = { peer ->
                        pendingPeer = peer
                        pickFile.launch("*/*")
                    },
                    onOpenFile = ::openFile,
                    onOpenUrl = { url ->
                        runCatching {
                            startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
                        }
                    },
                )
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        // Only stop when the activity is going for good, not on a rotation —
        // tearing the node down and back up would drop it off the network for
        // every peer each time the screen turns.
        if (isFinishing) repository.stop()
    }

    /**
     * Copy a picked file into app storage, then send it.
     *
     * A `content://` URI is not a filesystem path and the engine cannot open
     * it, so it has to be materialised first. The copy also outlives the
     * short-lived permission grant on the URI, which matters because the
     * transfer runs in the background.
     */
    private fun sendFromUri(peer: String, uri: Uri) {
        lifecycleScope.launch {
            runCatching {
                val name = contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                    val index = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                    if (index >= 0 && cursor.moveToFirst()) cursor.getString(index) else null
                } ?: "file"

                val outbox = File(filesDir, "outbox").apply { mkdirs() }
                val staged = File(outbox, name)
                contentResolver.openInputStream(uri)?.use { input ->
                    staged.outputStream().use { output -> input.copyTo(output) }
                }
                repository.sendFile(peer, staged.absolutePath)
            }
        }
    }

    /** Open a received file with whatever app handles it. */
    private fun openFile(path: String) {
        runCatching {
            val file = File(path)
            val uri = FileProvider.getUriForFile(this, "$packageName.files", file)
            startActivity(
                Intent(Intent.ACTION_VIEW).apply {
                    setDataAndType(uri, contentResolver.getType(uri) ?: "*/*")
                    addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                },
            )
        }
    }
}

// ─── Theme ───────────────────────────────────────────────────────────────────

/** Matching the web dashboard, so the two read as one product. */
private val SeriesOne = Color(0xFF2A78D6)
private val SeriesOneDark = Color(0xFF3987E5)
private val Good = Color(0xFF0CA30C)
private val Warning = Color(0xFFFAB219)
private val Critical = Color(0xFFD03B3B)

@Composable
fun BifrostTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    MaterialTheme(
        colorScheme = if (dark) {
            darkColorScheme(primary = SeriesOneDark, background = Color(0xFF0D0D0D), surface = Color(0xFF1A1A19))
        } else {
            lightColorScheme(primary = SeriesOne, background = Color(0xFFF9F9F7), surface = Color(0xFFFCFCFB))
        },
        content = content,
    )
}

// ─── App ─────────────────────────────────────────────────────────────────────

private enum class Tab(val label: String, val icon: ImageVector) {
    Devices("Devices", Icons.Filled.Devices),
    Chat("Chat", Icons.AutoMirrored.Filled.Chat),
    Files("Files", Icons.Filled.Folder),
    Settings("Settings", Icons.Filled.Settings),
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun BifrostApp(
    state: NodeState,
    repository: BifrostRepository,
    onPickFile: (String) -> Unit,
    onOpenFile: (String) -> Unit,
    onOpenUrl: (String) -> Unit,
) {
    var tab by remember { mutableStateOf(Tab.Devices) }

    if (state.error != null) {
        StartupFailure(state.error)
        return
    }
    if (!state.ready) {
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Column(horizontalAlignment = Alignment.CenterHorizontally) {
                CircularProgressIndicator()
                Spacer(Modifier.height(16.dp))
                Text("Joining the network…")
            }
        }
        return
    }

    Scaffold(
        bottomBar = {
            NavigationBar {
                Tab.entries.forEach { entry ->
                    NavigationBarItem(
                        selected = tab == entry,
                        onClick = { tab = entry },
                        icon = {
                            if (entry == Tab.Devices && state.onlinePeers.isNotEmpty()) {
                                BadgedBox(badge = { Badge { Text("${state.onlinePeers.size}") } }) {
                                    Icon(entry.icon, entry.label)
                                }
                            } else {
                                Icon(entry.icon, entry.label)
                            }
                        },
                        label = { Text(entry.label) },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.padding(padding)) {
            when (tab) {
                Tab.Devices -> DevicesScreen(state, repository)
                Tab.Chat -> ChatScreen(state, repository)
                Tab.Files -> FilesScreen(state, onPickFile, onOpenFile)
                Tab.Settings -> SettingsScreen(state, repository, onOpenUrl)
            }
        }
    }
}

@Composable
private fun StartupFailure(message: String) {
    Box(Modifier.fillMaxSize().padding(28.dp), contentAlignment = Alignment.Center) {
        Column(horizontalAlignment = Alignment.CenterHorizontally) {
            Icon(Icons.Filled.WifiOff, null, modifier = Modifier.size(48.dp))
            Spacer(Modifier.height(14.dp))
            Text("Bifrost could not start", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.height(8.dp))
            Text(message, textAlign = TextAlign.Center, style = MaterialTheme.typography.bodySmall)
        }
    }
}

// ─── Shared pieces ───────────────────────────────────────────────────────────

/**
 * A labelled bar with the value printed beside it.
 *
 * The number is always shown, so colour is a secondary cue rather than the
 * only one — the same rule the web dashboard follows.
 */
@Composable
fun Meter(label: String, fraction: Float, value: String) {
    val clamped = fraction.coerceIn(0f, 1f)
    val colour = when {
        clamped >= 0.9f -> Critical
        clamped >= 0.75f -> Warning
        else -> MaterialTheme.colorScheme.primary
    }

    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(vertical = 3.dp)) {
        Text(label, style = MaterialTheme.typography.bodySmall, modifier = Modifier.width(42.dp))
        LinearProgressIndicator(
            progress = { clamped },
            color = colour,
            trackColor = colour.copy(alpha = 0.18f),
            modifier = Modifier.weight(1f).height(8.dp),
        )
        Spacer(Modifier.width(10.dp))
        Text(
            value,
            style = MaterialTheme.typography.bodySmall,
            textAlign = TextAlign.End,
            modifier = Modifier.width(104.dp),
        )
    }
}

fun deviceIcon(deviceType: String): ImageVector {
    val d = deviceType.lowercase(Locale.ROOT)
    return when {
        d.contains("phone") || d.contains("android") || d.contains("ios") -> Icons.Filled.Smartphone
        d.contains("tablet") || d.contains("ipad") -> Icons.Filled.TabletMac
        d.contains("server") || d.contains("pi") -> Icons.Filled.Dns
        d.contains("mac") -> Icons.Filled.LaptopMac
        d.contains("windows") -> Icons.Filled.DesktopWindows
        d.contains("linux") -> Icons.Filled.Laptop
        else -> Icons.Filled.DeviceUnknown
    }
}

fun formatBytes(bytes: Long): String {
    if (bytes <= 0) return "0 B"
    val units = listOf("B", "KB", "MB", "GB", "TB")
    var value = bytes.toDouble()
    var unit = 0
    while (value >= 1024 && unit < units.lastIndex) {
        value /= 1024
        unit++
    }
    return if (value >= 100 || unit == 0) "${value.toInt()} ${units[unit]}"
    else String.format(Locale.ROOT, "%.1f %s", value, units[unit])
}

fun formatGb(mb: Long): String = String.format(Locale.ROOT, "%.1f GB", mb / 1024.0)

@Composable
fun SectionCard(title: String? = null, content: @Composable ColumnScope.() -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 5.dp),
        shape = RoundedCornerShape(12.dp),
    ) {
        Column(Modifier.padding(16.dp)) {
            if (title != null) {
                Text(title, style = MaterialTheme.typography.titleSmall)
                Spacer(Modifier.height(10.dp))
            }
            content()
        }
    }
}

// ─── Devices ─────────────────────────────────────────────────────────────────

@Composable
fun DevicesScreen(state: NodeState, repository: BifrostRepository) {
    var renaming by remember { mutableStateOf<Peer?>(null) }

    LazyColumn(Modifier.fillMaxSize().padding(vertical = 8.dp)) {
        item {
            SectionCard {
                Row(verticalAlignment = Alignment.Bottom) {
                    Text("${state.pool.nodes}", fontSize = 44.sp, fontWeight = FontWeight.SemiBold)
                    Spacer(Modifier.width(10.dp))
                    Text(
                        if (state.pool.nodes == 1) "device (just this one)" else "devices on this network",
                        modifier = Modifier.padding(bottom = 6.dp),
                    )
                }
                Spacer(Modifier.height(12.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(20.dp)) {
                    Stat("Cores", "${state.pool.cpuCores}")
                    Stat("RAM free", formatGb(state.pool.ramAvailableMb))
                    Stat("Disk free", "${state.pool.diskAvailableGb.toInt()} GB")
                }
            }
        }

        item {
            SectionCard {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(deviceIcon(state.deviceType), null, Modifier.size(20.dp))
                    Spacer(Modifier.width(9.dp))
                    Text(state.username, fontWeight = FontWeight.SemiBold, modifier = Modifier.weight(1f))
                    Text(state.ip, style = MaterialTheme.typography.bodySmall)
                }
                Spacer(Modifier.height(10.dp))
                Meter("CPU", (state.telemetry.cpuUsage / 100).toFloat(),
                    "${state.telemetry.cpuUsage.toInt()}% · ${state.telemetry.cpuCores} cores")
                Meter("RAM", state.telemetry.ramFraction,
                    "${formatGb(state.telemetry.ramUsedMb)} / ${formatGb(state.telemetry.ramTotalMb)}")
                Meter("Disk", state.telemetry.diskFraction,
                    "${state.telemetry.diskUsedGb.toInt()} / ${state.telemetry.diskTotalGb.toInt()} GB")
            }
        }

        item {
            Text(
                "Devices on this network",
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.padding(start = 16.dp, top = 12.dp, bottom = 4.dp),
            )
        }

        if (state.peers.isEmpty()) {
            item {
                SectionCard {
                    Text(
                        "No other devices yet.\n\nStart Bifrost on a laptop on this same Wi-Fi — " +
                            "it will appear here within a few seconds.",
                        textAlign = TextAlign.Center,
                        style = MaterialTheme.typography.bodyMedium,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        } else {
            items(state.peers, key = { it.nodeId }) { peer ->
                PeerCard(peer, repository, onRename = { renaming = peer })
            }
        }
    }

    renaming?.let { peer ->
        var draft by remember(peer.nodeId) { mutableStateOf(peer.alias ?: "") }
        AlertDialog(
            onDismissRequest = { renaming = null },
            title = { Text("Name for ${peer.username}") },
            text = {
                OutlinedTextField(
                    value = draft,
                    onValueChange = { draft = it },
                    placeholder = { Text("Leave empty to clear") },
                    singleLine = true,
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    repository.setAlias(peer.nodeId, draft.trim().ifBlank { null })
                    renaming = null
                }) { Text("Save") }
            },
            dismissButton = { TextButton(onClick = { renaming = null }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun Stat(label: String, value: String) {
    Column {
        Text(label, style = MaterialTheme.typography.bodySmall)
        Text(value, fontSize = 18.sp, fontWeight = FontWeight.SemiBold)
    }
}

@Composable
private fun PeerCard(peer: Peer, repository: BifrostRepository, onRename: () -> Unit) {
    SectionCard {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(deviceIcon(peer.deviceType), null, Modifier.size(20.dp))
            Spacer(Modifier.width(9.dp))
            Column(Modifier.weight(1f)) {
                Text(peer.displayName, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    buildString {
                        append(peer.ip).append(" · ").append(peer.deviceType)
                        peer.latencyMs?.let { append(" · ${it.toInt()} ms") }
                    },
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            // A dot plus the word, never colour alone.
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    Modifier.size(8.dp).background(
                        if (peer.online) Good else Color.Gray,
                        RoundedCornerShape(4.dp),
                    ),
                )
                Spacer(Modifier.width(5.dp))
                Text(if (peer.online) "Online" else "Offline", style = MaterialTheme.typography.bodySmall)
            }
        }

        Spacer(Modifier.height(8.dp))
        Meter("CPU", (peer.cpuUsage / 100).toFloat(), "${peer.cpuUsage.toInt()}%")
        Meter("RAM", peer.ramFraction, "${formatGb(peer.ramAvailableMb)} free")

        Row {
            TextButton(onClick = { repository.ping(peer.nodeId) }, enabled = peer.online) { Text("Ping") }
            TextButton(onClick = { repository.call(peer.nodeId) }, enabled = peer.online) { Text("Call") }
            TextButton(onClick = onRename) { Text("Rename") }
        }
    }
}

// ─── Chat ────────────────────────────────────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(state: NodeState, repository: BifrostRepository) {
    var draft by remember { mutableStateOf("") }
    var recipient by remember { mutableStateOf<String?>(null) }
    var menuOpen by remember { mutableStateOf(false) }

    // A device that goes offline must not stay selected.
    if (recipient != null && state.onlinePeers.none { it.nodeId == recipient }) recipient = null
    val recipientName = state.peers.firstOrNull { it.nodeId == recipient }?.displayName ?: "Everyone"

    Column(Modifier.fillMaxSize()) {
        Row(Modifier.padding(horizontal = 16.dp, vertical = 8.dp), verticalAlignment = Alignment.CenterVertically) {
            Text("To", style = MaterialTheme.typography.bodySmall)
            Spacer(Modifier.width(10.dp))
            Box {
                TextButton(onClick = { menuOpen = true }) {
                    Text(recipientName)
                    Icon(Icons.Filled.ArrowDropDown, null)
                }
                DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                    DropdownMenuItem(text = { Text("Everyone") }, onClick = { recipient = null; menuOpen = false })
                    state.onlinePeers.forEach { peer ->
                        DropdownMenuItem(
                            text = { Text(peer.displayName) },
                            onClick = { recipient = peer.nodeId; menuOpen = false },
                        )
                    }
                }
            }
        }

        val listState = androidx.compose.foundation.lazy.rememberLazyListState()
        LaunchedEffect(state.chats.size) {
            if (state.chats.isNotEmpty()) listState.animateScrollToItem(state.chats.lastIndex)
        }

        if (state.chats.isEmpty()) {
            Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                Text("No messages yet. Say hello.", style = MaterialTheme.typography.bodyMedium)
            }
        } else {
            LazyColumn(state = listState, modifier = Modifier.weight(1f).padding(horizontal = 12.dp)) {
                items(state.chats, key = { it.id }) { message ->
                    Bubble(message, mine = message.senderId == state.nodeId)
                }
            }
        }

        Row(
            Modifier.fillMaxWidth().padding(10.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            OutlinedTextField(
                value = draft,
                onValueChange = { draft = it },
                placeholder = {
                    Text(if (recipient == null) "Message everyone, or /help" else "Private message")
                },
                modifier = Modifier.weight(1f),
                maxLines = 4,
                keyboardActions = KeyboardActions(onSend = { }),
            )
            Spacer(Modifier.width(8.dp))
            FilledIconButton(
                onClick = {
                    val text = draft.trim()
                    if (text.isNotEmpty()) {
                        // A slash command is a command wherever it is typed.
                        if (recipient != null && !text.startsWith("/")) {
                            repository.directMessage(recipient!!, text)
                        } else {
                            repository.broadcast(text)
                        }
                        draft = ""
                    }
                },
                enabled = draft.isNotBlank(),
            ) { Icon(Icons.AutoMirrored.Filled.Send, "Send") }
        }
    }
}

@Composable
private fun Bubble(message: ChatMessage, mine: Boolean) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = 3.dp),
        horizontalArrangement = if (mine) Arrangement.End else Arrangement.Start,
    ) {
        Column(horizontalAlignment = if (mine) Alignment.End else Alignment.Start) {
            Text(
                buildString {
                    append(if (mine) "You" else message.senderName)
                    if (message.isDirect) append(" · private")
                },
                style = MaterialTheme.typography.bodySmall,
            )
            Surface(
                color = if (mine) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceVariant,
                shape = RoundedCornerShape(15.dp),
            ) {
                Text(
                    message.content,
                    modifier = Modifier.padding(horizontal = 13.dp, vertical = 8.dp),
                    color = if (mine) MaterialTheme.colorScheme.onPrimary else MaterialTheme.colorScheme.onSurface,
                )
            }
        }
    }
}

// ─── Files ───────────────────────────────────────────────────────────────────

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun FilesScreen(
    state: NodeState,
    onPickFile: (String) -> Unit,
    onOpenFile: (String) -> Unit,
) {
    var target by remember { mutableStateOf<String?>(null) }
    var menuOpen by remember { mutableStateOf(false) }

    if (target != null && state.onlinePeers.none { it.nodeId == target }) target = null
    val targetName = state.peers.firstOrNull { it.nodeId == target }?.displayName

    LazyColumn(Modifier.fillMaxSize().padding(vertical = 8.dp)) {
        item {
            SectionCard("Send a file") {
                Box {
                    OutlinedButton(onClick = { menuOpen = true }, enabled = state.onlinePeers.isNotEmpty()) {
                        Text(targetName ?: if (state.onlinePeers.isEmpty()) "No devices online" else "Choose a device")
                        Icon(Icons.Filled.ArrowDropDown, null)
                    }
                    DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                        state.onlinePeers.forEach { peer ->
                            DropdownMenuItem(
                                text = { Text(peer.displayName) },
                                onClick = { target = peer.nodeId; menuOpen = false },
                            )
                        }
                    }
                }
                Spacer(Modifier.height(10.dp))
                Button(
                    onClick = { target?.let(onPickFile) },
                    enabled = target != null,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Icon(Icons.Filled.UploadFile, null)
                    Spacer(Modifier.width(8.dp))
                    Text("Choose a file")
                }
            }
        }

        item {
            Text(
                "Transfers",
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.padding(start = 16.dp, top = 12.dp, bottom = 4.dp),
            )
        }

        if (state.transfers.isEmpty()) {
            item { SectionCard { Text("No transfers yet.", style = MaterialTheme.typography.bodyMedium) } }
        } else {
            items(state.transfers.take(25), key = { it.id }) { transfer ->
                TransferRow(transfer, onOpenFile)
            }
        }
    }
}

@Composable
private fun TransferRow(transfer: Transfer, onOpenFile: (String) -> Unit) {
    val colour = when {
        transfer.isFailed -> Critical
        transfer.isDone -> Good
        else -> MaterialTheme.colorScheme.primary
    }

    SectionCard {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(
                if (transfer.incoming) Icons.Filled.SouthEast else Icons.Filled.NorthEast,
                null,
                tint = colour,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(8.dp))
            Text(
                transfer.filename,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            // The label carries the state; colour only reinforces it.
            Text(transfer.label, style = MaterialTheme.typography.bodySmall)
        }
        Spacer(Modifier.height(4.dp))
        Text(
            "${transfer.peerName} · ${formatBytes(transfer.bytesDone)} of ${formatBytes(transfer.bytesTotal)}" +
                (transfer.detail?.let { " · $it" } ?: ""),
            style = MaterialTheme.typography.bodySmall,
        )
        Spacer(Modifier.height(6.dp))
        LinearProgressIndicator(
            progress = { transfer.progress },
            color = colour,
            trackColor = colour.copy(alpha = 0.18f),
            modifier = Modifier.fillMaxWidth(),
        )
        if (transfer.isDone && transfer.savedPath != null) {
            TextButton(onClick = { onOpenFile(transfer.savedPath) }) { Text("Open") }
        }
    }
}

// ─── Settings ────────────────────────────────────────────────────────────────

@Composable
fun SettingsScreen(state: NodeState, repository: BifrostRepository, onOpenUrl: (String) -> Unit) {
    var name by remember(state.username) { mutableStateOf(state.username) }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(vertical = 8.dp)) {
        val liveCalls = state.calls.filter { it.isLive }
        if (liveCalls.isNotEmpty()) {
            SectionCard("Active calls") {
                liveCalls.forEach { call ->
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Column(Modifier.weight(1f)) {
                            Text(call.peerName, fontWeight = FontWeight.Medium)
                            Text(
                                "${call.state} · ${call.framesSent} sent / ${call.framesReceived} received",
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                        TextButton(onClick = { repository.hangUp(call.sessionId) }) { Text("End") }
                    }
                }
            }
        }

        SectionCard("This device") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Display name") },
                    singleLine = true,
                    modifier = Modifier.weight(1f),
                )
                Spacer(Modifier.width(8.dp))
                TextButton(onClick = { if (name.isNotBlank()) repository.rename(name.trim()) }) { Text("Save") }
            }
            Spacer(Modifier.height(6.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Share resources", Modifier.weight(1f))
                Switch(checked = state.sharingEnabled, onCheckedChange = { repository.setSharing(it) })
            }
        }

        SectionCard("Connection") {
            InfoRow("Address", state.ip)
            InfoRow("Node id", state.nodeId.take(8))
            InfoRow("Version", "Bifrost ${state.version}")
            InfoRow("Downloads", state.downloadDir)

            if (state.onlinePeers.isNotEmpty()) {
                Spacer(Modifier.height(8.dp))
                Text(
                    "Open a laptop's dashboard in a browser:",
                    style = MaterialTheme.typography.bodySmall,
                )
                state.onlinePeers.forEach { peer ->
                    TextButton(onClick = { onOpenUrl(peer.dashboardUrl) }) { Text(peer.displayName) }
                }
            }
        }

        SectionCard("Activity") {
            if (state.log.isEmpty()) {
                Text("Nothing yet.", style = MaterialTheme.typography.bodySmall)
            } else {
                state.log.take(40).forEach { line ->
                    Text(
                        line.text,
                        style = MaterialTheme.typography.bodySmall,
                        color = when (line.level) {
                            "success" -> Good
                            "error" -> Critical
                            "warning" -> Warning
                            else -> MaterialTheme.colorScheme.onSurface
                        },
                        modifier = Modifier.padding(vertical = 1.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun InfoRow(label: String, value: String) {
    Row(Modifier.padding(vertical = 3.dp)) {
        Text(label, style = MaterialTheme.typography.bodySmall, modifier = Modifier.width(88.dp))
        Text(value, style = MaterialTheme.typography.bodySmall, modifier = Modifier.weight(1f))
    }
}
