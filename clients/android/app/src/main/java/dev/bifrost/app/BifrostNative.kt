package dev.bifrost.app

import org.json.JSONArray
import org.json.JSONObject

/**
 * The Rust engine.
 *
 * Every method here maps one-to-one onto a `Java_dev_bifrost_app_BifrostNative_*`
 * symbol in `crates/core/src/ffi_jni.rs`. The names must match exactly — JNI
 * resolves by mangled name, so renaming either side without the other fails at
 * call time rather than at build time.
 *
 * The surface is deliberately strings in, JSON out. Marshalling structs across
 * JNI means describing every field twice and keeping both in step forever;
 * one snapshot call that returns the whole state is far less to get wrong.
 *
 * Calls block. They are short — the engine does its real work on its own Tokio
 * threads — but [start] binds sockets and must not run on the UI thread. The
 * repository takes care of that.
 */
object BifrostNative {

    /** Thrown when the engine reports a failure. */
    class Failure(message: String) : Exception(message)

    init {
        System.loadLibrary("bifrost_core")
    }

    // ── Lifecycle ────────────────────────────────────────────────────────────

    private external fun nativeStart(configPath: String): Int
    private external fun nativeStop(): Int
    private external fun nativeIsRunning(): Int
    private external fun nativeLastError(): String?
    private external fun nativeVersion(): String?

    /**
     * Start the node, storing its config at [configPath].
     *
     * Must be an app-private path: Android gives an app no writable working
     * directory, so the engine's default location would fail.
     */
    fun start(configPath: String) {
        // 1 means "already running", which is success from the caller's point
        // of view — Android recreates activities freely.
        if (nativeStart(configPath) < 0) {
            throw Failure(lastError() ?: "could not start Bifrost")
        }
    }

    fun stop() {
        nativeStop()
    }

    val isRunning: Boolean get() = nativeIsRunning() != 0

    fun lastError(): String? = nativeLastError()?.takeIf { it.isNotBlank() }

    val version: String get() = nativeVersion() ?: "unknown"

    // ── Reading state ────────────────────────────────────────────────────────

    private external fun nativeSnapshotJson(): String?
    private external fun nativePollEventsJson(): String?

    /** The whole node in one consistent view, or null before it has started. */
    fun snapshot(): JSONObject? =
        nativeSnapshotJson()
            ?.takeIf { it != "null" }
            ?.let { runCatching { JSONObject(it) }.getOrNull() }

    /**
     * Events since the last call, oldest first.
     *
     * Polled rather than pushed: a callback from a Rust thread would need a
     * cached JNIEnv and an attached thread, and getting that wrong crashes the
     * process. Draining from the timer the UI already runs cannot.
     */
    fun pollEvents(): JSONArray =
        nativePollEventsJson()
            ?.let { runCatching { JSONArray(it) }.getOrNull() }
            ?: JSONArray()

    // ── Actions ──────────────────────────────────────────────────────────────

    private external fun nativeExecute(line: String): String?
    private external fun nativeSendChat(content: String, recipient: String?): Int
    private external fun nativeSendFile(peer: String, path: String): Int
    private external fun nativeSetAlias(peer: String, alias: String?): Int
    private external fun nativeSetSharing(enabled: Int): Int
    private external fun nativeRename(name: String): Int

    /**
     * Run a command line, exactly as the terminal dashboard would.
     *
     * Returns the human-readable result, so `/status` on a phone answers the
     * same as `/status` on a laptop.
     */
    fun execute(line: String): String {
        val raw = nativeExecute(line) ?: return ""
        if (raw == "null") return ""
        return runCatching { JSONObject(raw).optString("text") }.getOrDefault("")
    }

    fun sendChat(content: String, recipient: String? = null) =
        check(nativeSendChat(content, recipient), "send the message")

    fun sendFile(peer: String, path: String) =
        check(nativeSendFile(peer, path), "send the file")

    fun setAlias(peer: String, alias: String?) =
        check(nativeSetAlias(peer, alias), "rename that device")

    fun setSharing(enabled: Boolean) =
        check(nativeSetSharing(if (enabled) 1 else 0), "change sharing")

    fun rename(name: String) = check(nativeRename(name), "rename this device")

    // ── Voice ────────────────────────────────────────────────────────────────

    private external fun nativeCall(peer: String): String?
    private external fun nativeHangUp(session: String?): Int
    private external fun nativeSendAudio(session: String, pcm: ByteArray): Int
    private external fun nativeTakeAudio(session: String): String?

    /** Place a call, returning the session id. */
    fun call(peer: String): String {
        val session = nativeCall(peer)
        if (session.isNullOrBlank()) {
            throw Failure(lastError() ?: "could not place the call")
        }
        return session
    }

    fun hangUp(session: String? = null) = check(nativeHangUp(session), "end the call")

    /** Send one 20 ms frame of signed 16-bit little-endian mono PCM at 16 kHz. */
    fun sendAudio(session: String, pcm: ByteArray) {
        nativeSendAudio(session, pcm)
    }

    /** Drain received audio frames for a call. */
    fun takeAudio(session: String): List<ByteArray> {
        val raw = nativeTakeAudio(session) ?: return emptyList()
        val array = runCatching { JSONArray(raw) }.getOrNull() ?: return emptyList()

        return buildList {
            for (i in 0 until array.length()) {
                val decoded = runCatching {
                    android.util.Base64.decode(array.getString(i), android.util.Base64.DEFAULT)
                }.getOrNull()
                if (decoded != null) add(decoded)
            }
        }
    }

    private fun check(status: Int, what: String) {
        if (status < 0) throw Failure(lastError() ?: "could not $what")
    }
}
