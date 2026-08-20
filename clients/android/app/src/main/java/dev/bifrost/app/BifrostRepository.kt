package dev.bifrost.app

import android.content.Context
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.json.JSONObject
import java.io.File

/** One line in the activity log. */
data class LogLine(
    val text: String,
    val level: String,
    val at: Long = System.currentTimeMillis(),
)

/** Everything the UI renders, parsed from one snapshot. */
data class NodeState(
    val ready: Boolean = false,
    val error: String? = null,
    val username: String = "",
    val deviceType: String = "",
    val nodeId: String = "",
    val ip: String = "",
    val version: String = "",
    val sharingEnabled: Boolean = true,
    val downloadDir: String = "",
    val telemetry: Telemetry = Telemetry(),
    val pool: Pool = Pool(),
    val peers: List<Peer> = emptyList(),
    val chats: List<ChatMessage> = emptyList(),
    val transfers: List<Transfer> = emptyList(),
    val calls: List<Call> = emptyList(),
    val log: List<LogLine> = emptyList(),
) {
    val onlinePeers: List<Peer> get() = peers.filter { it.online }
    val activeCall: Call? get() = calls.firstOrNull { it.state == "connected" }
}

data class Telemetry(
    val cpuUsage: Double = 0.0,
    val cpuCores: Int = 0,
    val ramUsedMb: Long = 0,
    val ramTotalMb: Long = 0,
    val diskUsedGb: Double = 0.0,
    val diskTotalGb: Double = 0.0,
) {
    val ramFraction: Float get() = if (ramTotalMb == 0L) 0f else (ramUsedMb.toFloat() / ramTotalMb)
    val diskFraction: Float get() = if (diskTotalGb == 0.0) 0f else (diskUsedGb / diskTotalGb).toFloat()
    val ramAvailableMb: Long get() = (ramTotalMb - ramUsedMb).coerceAtLeast(0)
}

data class Pool(
    val nodes: Int = 0,
    val cpuCores: Int = 0,
    val ramAvailableMb: Long = 0,
    val diskAvailableGb: Double = 0.0,
    val sharingNodes: Int = 0,
)

data class Peer(
    val nodeId: String,
    val username: String,
    val alias: String?,
    val ip: String,
    val deviceType: String,
    val online: Boolean,
    val cpuUsage: Double,
    val ramAvailableMb: Long,
    val ramFraction: Float,
    val latencyMs: Double?,
    val dashboardUrl: String,
) {
    val displayName: String get() = alias?.takeIf { it.isNotBlank() } ?: username
}

data class ChatMessage(
    val id: String,
    val senderId: String,
    val senderName: String,
    val recipientId: String?,
    val content: String,
    val timestamp: String,
) {
    val isDirect: Boolean get() = recipientId != null
}

data class Transfer(
    val id: String,
    val peerName: String,
    val filename: String,
    val incoming: Boolean,
    val state: String,
    val detail: String?,
    val bytesTotal: Long,
    val bytesDone: Long,
    val savedPath: String?,
) {
    val progress: Float
        get() = if (bytesTotal == 0L) (if (state == "completed") 1f else 0f)
        else (bytesDone.toFloat() / bytesTotal).coerceIn(0f, 1f)

    val isDone: Boolean get() = state == "completed"
    val isFailed: Boolean get() = state == "failed" || state == "rejected"

    val label: String
        get() = when (state) {
            "offered" -> "Offered"
            "accepted" -> "Accepted"
            "streaming" -> "Transferring"
            "verifying" -> "Verifying"
            "completed" -> "Done"
            "rejected" -> "Declined"
            "failed" -> "Failed"
            else -> state
        }
}

data class Call(
    val sessionId: String,
    val peerId: String,
    val peerName: String,
    val outgoing: Boolean,
    val state: String,
    val framesSent: Long,
    val framesReceived: Long,
) {
    val isLive: Boolean get() = state == "ringing" || state == "connected"
}

/**
 * Owns the engine and turns it into a [StateFlow] the UI observes.
 *
 * One snapshot per tick rather than a call per field: a UI that samples pieces
 * at different moments draws a frame that never existed — a peer count from
 * before a disconnection beside telemetry from after it.
 */
class BifrostRepository(private val context: Context) {

    private val _state = MutableStateFlow(NodeState())
    val state: StateFlow<NodeState> = _state.asStateFlow()

    private val log = ArrayDeque<LogLine>()
    private var polling: CoroutineScope? = null

    /** Start the engine. Safe to call again after a configuration change. */
    suspend fun start(scope: CoroutineScope) {
        if (BifrostNative.isRunning) {
            beginPolling(scope)
            return
        }

        // Binding sockets blocks; keep it off the main thread.
        val result = withContext(Dispatchers.IO) {
            runCatching {
                val dir = File(context.filesDir, "bifrost").apply { mkdirs() }
                BifrostNative.start(File(dir, "config.json").absolutePath)
            }
        }

        result.fold(
            onSuccess = {
                push("Bifrost started", "success")
                beginPolling(scope)
            },
            onFailure = { e ->
                _state.value = _state.value.copy(
                    ready = false,
                    error = e.message ?: "Bifrost could not start",
                )
            },
        )
    }

    fun stop() {
        polling = null
        runCatching { BifrostNative.stop() }
    }

    private fun beginPolling(scope: CoroutineScope) {
        if (polling != null) return
        polling = scope

        scope.launch(Dispatchers.IO) {
            while (true) {
                drainEvents()
                refresh()
                delay(POLL_INTERVAL_MS)
            }
        }
    }

    /** Pull a snapshot now, so the UI reacts to an action immediately. */
    fun refresh() {
        val snapshot = runCatching { BifrostNative.snapshot() }.getOrNull() ?: return
        _state.value = parse(snapshot).copy(log = log.toList())
    }

    private fun drainEvents() {
        val events = runCatching { BifrostNative.pollEvents() }.getOrNull() ?: return

        for (i in 0 until events.length()) {
            val event = events.optJSONObject(i) ?: continue
            val data = event.optJSONObject("data") ?: JSONObject()

            when (event.optString("event")) {
                "notice" -> push(data.optString("text"), data.optString("level", "info"))
                "chat" -> {
                    // Our own messages were shown when we sent them.
                    if (data.optString("sender_id") != _state.value.nodeId) {
                        push("${data.optString("sender_name")}: ${data.optString("content")}", "info")
                    }
                }
                "peer_online" -> push("${data.optString("username")} joined", "success")
                "peer_offline" -> push("${data.optString("display_name")} left", "warning")
            }
        }
    }

    private fun push(text: String, level: String) {
        if (text.isBlank()) return
        log.addFirst(LogLine(text, level))
        while (log.size > MAX_LOG) log.removeLast()
        _state.value = _state.value.copy(log = log.toList())
    }

    // ── Actions ──────────────────────────────────────────────────────────────
    //
    // Each reports failure into the log rather than throwing into a Compose
    // callback, where it would become an unhandled crash.

    private inline fun guard(what: String, block: () -> Unit) {
        runCatching(block).onFailure { push("$what failed: ${it.message}", "error") }
        refresh()
    }

    fun broadcast(message: String) {
        if (message.isBlank()) return
        // A slash command is a command wherever it is typed.
        if (message.startsWith("/")) {
            guard("Command") {
                val reply = BifrostNative.execute(message)
                if (reply.isNotBlank()) push(reply, "info")
            }
        } else {
            guard("Sending") { BifrostNative.sendChat(message.trim()) }
        }
    }

    fun directMessage(peerId: String, message: String) =
        guard("Sending") { BifrostNative.sendChat(message.trim(), peerId) }

    fun sendFile(peerId: String, path: String) =
        guard("Sending the file") { BifrostNative.sendFile(peerId, path) }

    fun setAlias(peerId: String, alias: String?) =
        guard("Renaming") { BifrostNative.setAlias(peerId, alias) }

    fun setSharing(enabled: Boolean) =
        guard("Changing sharing") { BifrostNative.setSharing(enabled) }

    fun rename(name: String) = guard("Renaming") { BifrostNative.rename(name) }

    fun ping(peerId: String) = guard("Ping") {
        val reply = BifrostNative.execute("/ping $peerId")
        if (reply.isNotBlank()) push(reply, "info")
    }

    fun call(peerId: String) = guard("Calling") {
        val session = BifrostNative.call(peerId)
        push("In a call (session ${session.take(8)})", "success")
    }

    fun hangUp(session: String? = null) = guard("Ending the call") {
        BifrostNative.hangUp(session)
    }

    // ── Parsing ──────────────────────────────────────────────────────────────

    private fun parse(json: JSONObject): NodeState {
        val identity = json.optJSONObject("identity") ?: JSONObject()
        val telemetry = json.optJSONObject("telemetry") ?: JSONObject()
        val pool = json.optJSONObject("pool") ?: JSONObject()

        val peers = json.optJSONArray("peers").mapObjects { p ->
            val t = p.optJSONObject("telemetry") ?: JSONObject()
            val total = t.optLong("ram_total_mb")
            val used = t.optLong("ram_used_mb")
            val ports = p.optJSONObject("ports") ?: JSONObject()
            val ip = p.optString("ip")

            Peer(
                nodeId = p.optString("node_id"),
                username = p.optString("username"),
                alias = p.optStringOrNull("alias"),
                ip = ip,
                deviceType = p.optString("device_type"),
                online = p.optBoolean("online"),
                cpuUsage = t.optDouble("cpu_usage", 0.0),
                ramAvailableMb = (total - used).coerceAtLeast(0),
                ramFraction = if (total == 0L) 0f else used.toFloat() / total,
                latencyMs = if (p.isNull("latency_ms")) null else p.optDouble("latency_ms"),
                dashboardUrl = "http://$ip:${ports.optInt("web", 9000)}",
            )
        }

        val chats = json.optJSONArray("chats").mapObjects { m ->
            ChatMessage(
                id = m.optString("message_id"),
                senderId = m.optString("sender_id"),
                senderName = m.optString("sender_name"),
                recipientId = m.optStringOrNull("recipient_id"),
                content = m.optString("content"),
                timestamp = m.optString("timestamp"),
            )
        }

        val transfers = json.optJSONArray("transfers").mapObjects { t ->
            val state = t.optJSONObject("state") ?: JSONObject()
            Transfer(
                id = t.optString("transfer_id"),
                peerName = t.optString("peer_name"),
                filename = t.optString("filename"),
                incoming = t.optString("direction") == "incoming",
                state = state.optString("state"),
                detail = state.optStringOrNull("detail"),
                bytesTotal = t.optLong("bytes_total"),
                bytesDone = t.optLong("bytes_done"),
                savedPath = t.optStringOrNull("saved_path"),
            )
        }

        val calls = json.optJSONArray("calls").mapObjects { c ->
            Call(
                sessionId = c.optString("session_id"),
                peerId = c.optString("peer_id"),
                peerName = c.optString("peer_name"),
                outgoing = c.optBoolean("outgoing"),
                state = c.optString("state"),
                framesSent = c.optLong("frames_sent"),
                framesReceived = c.optLong("frames_received"),
            )
        }

        return NodeState(
            ready = true,
            error = null,
            username = identity.optString("username"),
            deviceType = identity.optString("device_type"),
            nodeId = identity.optString("node_id"),
            ip = json.optString("ip"),
            version = json.optString("version"),
            sharingEnabled = json.optBoolean("sharing_enabled", true),
            downloadDir = json.optString("download_dir"),
            telemetry = Telemetry(
                cpuUsage = telemetry.optDouble("cpu_usage", 0.0),
                cpuCores = telemetry.optInt("cpu_cores"),
                ramUsedMb = telemetry.optLong("ram_used_mb"),
                ramTotalMb = telemetry.optLong("ram_total_mb"),
                diskUsedGb = telemetry.optDouble("disk_used_gb", 0.0),
                diskTotalGb = telemetry.optDouble("disk_total_gb", 0.0),
            ),
            pool = Pool(
                nodes = pool.optInt("nodes"),
                cpuCores = pool.optInt("cpu_cores"),
                ramAvailableMb = pool.optLong("ram_available_mb"),
                diskAvailableGb = pool.optDouble("disk_available_gb", 0.0),
                sharingNodes = pool.optInt("sharing_nodes"),
            ),
            peers = peers,
            chats = chats,
            transfers = transfers,
            calls = calls,
        )
    }

    private companion object {
        const val POLL_INTERVAL_MS = 900L
        const val MAX_LOG = 200
    }
}

// ── JSON helpers ─────────────────────────────────────────────────────────────

private fun JSONObject.optStringOrNull(key: String): String? =
    if (isNull(key)) null else optString(key).takeIf { it.isNotBlank() }

private inline fun <T> org.json.JSONArray?.mapObjects(transform: (JSONObject) -> T): List<T> {
    if (this == null) return emptyList()
    return buildList {
        for (i in 0 until length()) {
            optJSONObject(i)?.let { add(transform(it)) }
        }
    }
}
