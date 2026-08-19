/*
 * bifrost_core.h — C ABI for the Bifrost engine.
 *
 * Mirrors `crates/core/src/ffi.rs`. Used by the SwiftUI app (via a bridging
 * header) and available to any other C-compatible caller.
 *
 * Two rules govern the whole surface:
 *
 *   1. Every `char *` returned by this library is owned by the caller and must
 *      be released with bifrost_string_free(). A NULL return means failure;
 *      bifrost_last_error() says why.
 *   2. Functions returning int use 0 for success and -1 for failure, except
 *      where noted.
 *
 * No function here can panic across the boundary: the Rust side catches and
 * reports instead.
 *
 * Build:
 *   cargo build --release --target aarch64-apple-ios
 *   -> target/aarch64-apple-ios/release/libbifrost_core.a
 */

#ifndef BIFROST_CORE_H
#define BIFROST_CORE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Lifecycle ─────────────────────────────────────────────────────────── */

/*
 * Start the node.
 *
 * config_path may be NULL for the platform default. On iOS pass a path inside
 * the app's Documents directory — the working directory is not writable.
 *
 * Returns 0 on success, 1 if a node was already running, -1 on failure.
 */
int32_t bifrost_start(const char *config_path);

/* Stop the node. Safe to call when nothing is running. */
int32_t bifrost_stop(void);

/* 1 if a node is running, 0 otherwise. */
int32_t bifrost_is_running(void);

/* Most recent error, or an empty string. Caller frees. */
char *bifrost_last_error(void);

/* Library version, e.g. "1.0.0". Caller frees. */
char *bifrost_version(void);

/* Wire protocol version this build speaks. */
int32_t bifrost_protocol_version(void);

/* Release a string returned by this library. NULL is accepted. */
void bifrost_string_free(char *ptr);

/* ── Reading state ─────────────────────────────────────────────────────── */

/* This node's id. Caller frees. */
char *bifrost_node_id(void);

/*
 * A complete snapshot as JSON: identity, telemetry, peers, chat, transfers
 * and calls in one consistent view. The single call a UI needs per frame.
 * Caller frees.
 */
char *bifrost_snapshot_json(void);

/* JSON arrays of the individual collections. Caller frees. */
char *bifrost_peers_json(void);
char *bifrost_chat_json(int32_t limit);
char *bifrost_transfers_json(void);
char *bifrost_calls_json(void);

/*
 * Drain buffered events as a JSON array, oldest first; "[]" when idle.
 * Poll this from the UI's existing refresh timer. Caller frees.
 */
char *bifrost_poll_events(void);

/* ── Actions ───────────────────────────────────────────────────────────── */

/*
 * Run a command line exactly as the terminal dashboard would — plain text to
 * broadcast, or any slash command. Returns the outcome as JSON, or the string
 * "null" for an empty line. Caller frees.
 */
char *bifrost_execute(const char *line);

/* Send a chat message. recipient may be NULL to broadcast. */
int32_t bifrost_send_chat(const char *content, const char *recipient);

/* Send a file to a peer (node id, id prefix, username or alias). */
int32_t bifrost_send_file(const char *peer, const char *path);

/* Nickname a peer. alias may be NULL to clear it. */
int32_t bifrost_set_alias(const char *peer, const char *alias);

/* Turn resource sharing on (non-zero) or off (zero). */
int32_t bifrost_set_sharing(int32_t enabled);

/* Change this node's display name. */
int32_t bifrost_rename(const char *name);

/* ── Voice ─────────────────────────────────────────────────────────────── */

/* Place a call. Returns the session id, or NULL on failure. Caller frees. */
char *bifrost_call(const char *peer);

/* End a call. session may be NULL to end the only active one. */
int32_t bifrost_hangup(const char *session);

/*
 * Send one captured audio frame: signed 16-bit little-endian mono at 16 kHz.
 * Call roughly every 20 ms with 320 samples (640 bytes).
 */
int32_t bifrost_voice_send_frame(const char *session, const uint8_t *pcm, size_t len);

/*
 * Drain received audio for a call as a JSON array of base64 frames.
 * Base64 because PCM contains NUL bytes, which would truncate a C string.
 * Caller frees.
 */
char *bifrost_voice_take_frames(const char *session);

#ifdef __cplusplus
}
#endif

#endif /* BIFROST_CORE_H */
