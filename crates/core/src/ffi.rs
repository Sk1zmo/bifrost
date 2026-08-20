//! C ABI for the Android and iOS clients.
//!
//! Flutter reaches this through `dart:ffi` and Swift links it directly. The
//! surface is deliberately narrow — strings in, JSON out — because every extra
//! type crossing the boundary is a struct layout that has to be mirrored by
//! hand in two more languages and kept in step forever.
//!
//! Three rules hold throughout:
//!
//! * **Every returned string is owned by the caller** and must be released
//!   with [`bifrost_string_free`]. `NULL` means failure; ask
//!   [`bifrost_last_error`] why.
//! * **No panic escapes.** Unwinding across an FFI boundary is undefined
//!   behaviour, so every entry point catches, records and reports instead.
//! * **Events are polled, not pushed.** [`bifrost_poll_events`] drains a
//!   buffer. Calling back into Dart or Swift from a Rust thread means
//!   isolate ports and run loops; polling from the UI thread's existing timer
//!   is simpler and cannot deadlock.
//!
//! ```c
//! bifrost_start(NULL);
//! char *json = bifrost_snapshot_json();
//! /* … use json … */
//! bifrost_string_free(json);
//! bifrost_stop();
//! ```

use std::ffi::{c_char, c_int, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use tokio::runtime::Runtime;

use crate::events::NodeEvent;
use crate::net::protocol::AudioCodec;
use crate::node::Node;
use crate::util::config::Config;

/// Events buffered for [`bifrost_poll_events`] before the oldest is dropped.
const EVENT_BUFFER: usize = 256;

// ─── Process-wide state ──────────────────────────────────────────────────────

pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("the Bifrost FFI could not start a tokio runtime"))
}

fn node_slot() -> &'static Mutex<Option<Node>> {
    static NODE: OnceLock<Mutex<Option<Node>>> = OnceLock::new();
    NODE.get_or_init(|| Mutex::new(None))
}

fn event_buffer() -> &'static Mutex<std::collections::VecDeque<NodeEvent>> {
    static EVENTS: OnceLock<Mutex<std::collections::VecDeque<NodeEvent>>> = OnceLock::new();
    EVENTS.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

fn last_error() -> &'static Mutex<String> {
    static ERROR: OnceLock<Mutex<String>> = OnceLock::new();
    ERROR.get_or_init(|| Mutex::new(String::new()))
}

fn set_error(message: impl Into<String>) {
    let message = message.into();
    tracing::warn!(error = %message, "FFI call failed");
    if let Ok(mut slot) = last_error().lock() {
        *slot = message;
    }
}

fn clear_error() {
    if let Ok(mut slot) = last_error().lock() {
        slot.clear();
    }
}

/// Run a closure with the live node, or record why it could not.
fn with_node<T>(f: impl FnOnce(&Node) -> T) -> Option<T> {
    match node_slot().lock() {
        Ok(slot) => match slot.as_ref() {
            Some(node) => Some(f(node)),
            None => {
                set_error("no node is running; call bifrost_start first");
                None
            }
        },
        Err(_) => {
            set_error("the node lock is poisoned; restart the app");
            None
        }
    }
}

// ─── Shared implementation ───────────────────────────────────────────────────
//
// Both ABIs — the C one below and the JNI one in `ffi_jni` — are thin shells
// over these. Keeping one implementation is what stops Android and iOS
// drifting apart in behaviour as the node changes underneath them.

// Used by the JNI layer, which only exists on Android.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn record_error(message: impl Into<String>) {
    set_error(message);
}

pub(crate) fn last_error_text() -> String {
    last_error().lock().map(|e| e.clone()).unwrap_or_default()
}

pub(crate) fn node_is_running() -> bool {
    node_slot()
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// Install a freshly started node, and begin buffering its events.
pub(crate) fn install_node(node: Node) {
    let mut rx = node.subscribe();
    runtime().spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(mut buffer) = event_buffer().lock() {
                        buffer.push_back(event);
                        while buffer.len() > EVENT_BUFFER {
                            buffer.pop_front();
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    if let Ok(mut slot) = node_slot().lock() {
        *slot = Some(node);
    }
}

pub(crate) fn shutdown_node() {
    let node = node_slot().lock().ok().and_then(|mut slot| slot.take());
    if let Some(node) = node {
        runtime().block_on(node.shutdown());
    }
    if let Ok(mut buffer) = event_buffer().lock() {
        buffer.clear();
    }
}

/// Serialise something derived from the live node, or `"null"`.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn with_node_json<T: serde::Serialize>(f: impl FnOnce(&Node) -> T) -> String {
    match with_node(f) {
        Some(value) => serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
        None => "null".into(),
    }
}

pub(crate) fn drain_events_json() -> String {
    let events: Vec<NodeEvent> = match event_buffer().lock() {
        Ok(mut buffer) => buffer.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    serde_json::to_string(&events).unwrap_or_else(|_| "[]".into())
}

/// Run a command line, returning the [`CommandOutcome`] as JSON.
///
/// The outcome is serialised flat — `{"outcome":"message","level":…,"text":…}`
/// — because that is what `bifrost_core.h` documents and what the Swift and
/// Kotlin clients read. Wrapping it in another object would silently change
/// the published C ABI.
///
/// [`CommandOutcome`]: crate::node::CommandOutcome
pub(crate) fn execute_line_json(line: &str) -> String {
    match with_node(|node| runtime().block_on(node.execute_line(line))) {
        Some(Ok(Some(outcome))) => {
            serde_json::to_string(&outcome).unwrap_or_else(|_| "null".into())
        }
        Some(Ok(None)) => "null".into(),
        Some(Err(e)) => {
            set_error(format!("{e:#}"));
            "null".into()
        }
        None => "null".into(),
    }
}

/// Turn an action's result into the 0 / -1 convention both ABIs use.
fn status<T>(result: Option<anyhow::Result<T>>) -> c_int {
    match result {
        Some(Ok(_)) => 0,
        Some(Err(e)) => {
            set_error(format!("{e:#}"));
            -1
        }
        None => -1,
    }
}

pub(crate) fn send_chat(content: &str, recipient: Option<&str>) -> c_int {
    clear_error();
    status(with_node(|node| {
        runtime().block_on(async {
            match recipient {
                Some(peer) => node.direct_message(peer, content).await,
                None => node.broadcast(content).await,
            }
        })
    }))
}

pub(crate) fn send_file(peer: &str, path: &str) -> c_int {
    clear_error();
    status(with_node(|node| {
        runtime().block_on(node.send_file(peer, std::path::Path::new(path)))
    }))
}

pub(crate) fn set_alias(peer: &str, alias: Option<String>) -> c_int {
    clear_error();
    status(with_node(|node| {
        runtime().block_on(node.set_alias(peer, alias))
    }))
}

pub(crate) fn set_sharing(enabled: bool) -> c_int {
    clear_error();
    status(with_node(|node| {
        runtime().block_on(node.set_sharing(enabled))
    }))
}

pub(crate) fn rename(name: &str) -> c_int {
    clear_error();
    status(with_node(|node| runtime().block_on(node.rename(name))))
}

/// Place a call, returning the session id.
pub(crate) fn start_call(peer: &str) -> Option<String> {
    clear_error();
    let result = with_node(|node| {
        runtime().block_on(async {
            let resolved = node.registry.resolve(peer).await?;
            node.voice
                .call(&node.registry, &resolved.node_id)
                .await
                .ok()
        })
    });

    let session = result.flatten();
    if session.is_none() && last_error_text().is_empty() {
        set_error(format!("could not call \"{peer}\""));
    }
    session
}

pub(crate) fn hang_up(session: Option<&str>) -> c_int {
    clear_error();
    status(with_node(|node| runtime().block_on(node.hang_up(session))))
}

pub(crate) fn send_audio_frame(session: &str, pcm: &[u8]) -> c_int {
    status(with_node(|node| {
        runtime().block_on(node.voice.send_frame(session, AudioCodec::Pcm16Le, pcm))
    }))
}

/// Drain received audio as a JSON array of base64 frames.
pub(crate) fn take_audio_json(session: &str) -> String {
    use base64::Engine as _;

    match with_node(|node| runtime().block_on(node.voice.take_incoming(session))) {
        Some(frames) => {
            let encoded: Vec<String> = frames
                .iter()
                .map(|f| base64::engine::general_purpose::STANDARD.encode(f))
                .collect();
            serde_json::to_string(&encoded).unwrap_or_else(|_| "[]".into())
        }
        None => "[]".into(),
    }
}

// ─── Conversions ─────────────────────────────────────────────────────────────

/// Borrow a C string as `String`. `NULL` and invalid UTF-8 both yield `None`.
///
/// # Safety
/// `ptr` must be null or a valid NUL-terminated C string that stays alive for
/// the duration of the call.
unsafe fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Hand a Rust string to the caller, who must free it.
fn out_string(value: impl Into<Vec<u8>>) -> *mut c_char {
    match CString::new(value) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            // An interior NUL byte cannot be represented in C.
            set_error("the result contained an interior NUL byte");
            std::ptr::null_mut()
        }
    }
}

fn out_json<T: serde::Serialize>(value: &T) -> *mut c_char {
    match serde_json::to_string(value) {
        Ok(json) => out_string(json),
        Err(e) => {
            set_error(format!("could not serialise the result: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Wrap an entry point that returns a string, catching any panic.
fn guard_string(name: &str, f: impl FnOnce() -> *mut c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(ptr) => ptr,
        Err(_) => {
            set_error(format!("{name} panicked"));
            std::ptr::null_mut()
        }
    }
}

/// Wrap an entry point that returns a status code, catching any panic.
///
/// Returns `0` on success and `-1` on failure, the convention throughout.
fn guard_int(name: &str, f: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            set_error(format!("{name} panicked"));
            -1
        }
    }
}

// ─── Lifecycle ───────────────────────────────────────────────────────────────

/// Start the node. `config_path` may be `NULL` for the default location.
///
/// Returns `0` on success, `-1` on failure, and `1` if already running.
///
/// On Android and iOS, pass the app's writable documents directory: the
/// working directory is not writable on either platform, so the default path
/// would fail.
///
/// # Safety
/// `config_path` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn bifrost_start(config_path: *const c_char) -> c_int {
    guard_int("bifrost_start", || {
        clear_error();

        if node_is_running() {
            return 1;
        }

        let path = unsafe { cstr(config_path) }
            .map(PathBuf::from)
            .unwrap_or_else(Config::default_path);

        let config = match Config::load_or_create(&path) {
            Ok(config) => config,
            Err(e) => {
                set_error(format!("could not load the config: {e:#}"));
                return -1;
            }
        };

        match runtime().block_on(Node::start(config, &path)) {
            Ok(node) => {
                install_node(node);
                0
            }
            Err(e) => {
                set_error(format!("could not start the node: {e:#}"));
                -1
            }
        }
    })
}

/// Stop the node. Safe to call when nothing is running.
#[no_mangle]
pub extern "C" fn bifrost_stop() -> c_int {
    guard_int("bifrost_stop", || {
        clear_error();
        shutdown_node();
        0
    })
}

/// `1` if a node is running, otherwise `0`.
#[no_mangle]
pub extern "C" fn bifrost_is_running() -> c_int {
    guard_int("bifrost_is_running", || c_int::from(node_is_running()))
}

/// The most recent error, or an empty string. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_last_error() -> *mut c_char {
    guard_string("bifrost_last_error", || out_string(last_error_text()))
}

/// This build's version. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_version() -> *mut c_char {
    guard_string("bifrost_version", || out_string(crate::VERSION))
}

/// The wire protocol version this build speaks.
#[no_mangle]
pub extern "C" fn bifrost_protocol_version() -> c_int {
    c_int::from(crate::PROTOCOL_VERSION)
}

/// Release a string returned by this library.
///
/// # Safety
/// `ptr` must be null, or a pointer this library returned and that has not
/// already been freed.
#[no_mangle]
pub unsafe extern "C" fn bifrost_string_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(ptr) });
}

// ─── Reading state ───────────────────────────────────────────────────────────

/// This node's id. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_node_id() -> *mut c_char {
    guard_string("bifrost_node_id", || {
        match with_node(|node| node.node_id()) {
            Some(id) => out_string(id),
            None => std::ptr::null_mut(),
        }
    })
}

/// A full [`NodeSnapshot`](crate::node::NodeSnapshot) as JSON. Caller frees.
///
/// The single call a mobile UI needs per frame: identity, telemetry, peers,
/// chat, transfers and calls in one consistent view.
#[no_mangle]
pub extern "C" fn bifrost_snapshot_json() -> *mut c_char {
    guard_string("bifrost_snapshot_json", || {
        match with_node(|node| runtime().block_on(node.snapshot())) {
            Some(snapshot) => out_json(&snapshot),
            None => std::ptr::null_mut(),
        }
    })
}

/// The peer list as a JSON array. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_peers_json() -> *mut c_char {
    guard_string("bifrost_peers_json", || {
        match with_node(|node| runtime().block_on(node.registry.list())) {
            Some(peers) => out_json(&peers),
            None => std::ptr::null_mut(),
        }
    })
}

/// The most recent `limit` chat messages as a JSON array. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_chat_json(limit: c_int) -> *mut c_char {
    guard_string("bifrost_chat_json", || {
        let limit = limit.clamp(1, 1000) as usize;
        match with_node(|node| runtime().block_on(node.chat.recent(limit))) {
            Some(messages) => out_json(&messages),
            None => std::ptr::null_mut(),
        }
    })
}

/// Every transfer as a JSON array. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_transfers_json() -> *mut c_char {
    guard_string("bifrost_transfers_json", || {
        match with_node(|node| runtime().block_on(node.transfers.list())) {
            Some(transfers) => out_json(&transfers),
            None => std::ptr::null_mut(),
        }
    })
}

/// Every call as a JSON array. Caller frees.
#[no_mangle]
pub extern "C" fn bifrost_calls_json() -> *mut c_char {
    guard_string("bifrost_calls_json", || {
        match with_node(|node| runtime().block_on(node.voice.sessions())) {
            Some(calls) => out_json(&calls),
            None => std::ptr::null_mut(),
        }
    })
}

/// Drain buffered events as a JSON array, oldest first. Caller frees.
///
/// Poll from the UI's existing refresh timer. Returns `[]` when idle.
#[no_mangle]
pub extern "C" fn bifrost_poll_events() -> *mut c_char {
    guard_string("bifrost_poll_events", || out_string(drain_events_json()))
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Run a command line exactly as the terminal dashboard would.
///
/// Accepts plain text (broadcast) or any slash command. Returns the outcome as
/// JSON, or the string `"null"` for an empty line. Caller frees.
///
/// # Safety
/// `line` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn bifrost_execute(line: *const c_char) -> *mut c_char {
    guard_string("bifrost_execute", || {
        clear_error();
        let Some(line) = (unsafe { cstr(line) }) else {
            set_error("bifrost_execute needs a valid UTF-8 string");
            return std::ptr::null_mut();
        };
        out_string(execute_line_json(&line))
    })
}

/// Send a chat message. `recipient` may be `NULL` to broadcast.
///
/// # Safety
/// Both pointers must be null or valid NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn bifrost_send_chat(
    content: *const c_char,
    recipient: *const c_char,
) -> c_int {
    guard_int("bifrost_send_chat", || {
        clear_error();
        let Some(content) = (unsafe { cstr(content) }) else {
            set_error("bifrost_send_chat needs a message");
            return -1;
        };
        send_chat(&content, unsafe { cstr(recipient) }.as_deref())
    })
}

/// Send a file to a peer.
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn bifrost_send_file(peer: *const c_char, path: *const c_char) -> c_int {
    guard_int("bifrost_send_file", || {
        clear_error();
        let (Some(peer), Some(path)) = (unsafe { cstr(peer) }, unsafe { cstr(path) }) else {
            set_error("bifrost_send_file needs a peer and a path");
            return -1;
        };
        send_file(&peer, &path)
    })
}

/// Nickname a peer. `alias` may be `NULL` to clear it.
///
/// # Safety
/// Both pointers must be null or valid NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn bifrost_set_alias(peer: *const c_char, alias: *const c_char) -> c_int {
    guard_int("bifrost_set_alias", || {
        clear_error();
        let Some(peer) = (unsafe { cstr(peer) }) else {
            set_error("bifrost_set_alias needs a peer");
            return -1;
        };
        set_alias(&peer, unsafe { cstr(alias) })
    })
}

/// Turn resource sharing on (non-zero) or off (zero).
#[no_mangle]
pub extern "C" fn bifrost_set_sharing(enabled: c_int) -> c_int {
    guard_int("bifrost_set_sharing", || set_sharing(enabled != 0))
}

/// Change this node's display name.
///
/// # Safety
/// `name` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn bifrost_rename(name: *const c_char) -> c_int {
    guard_int("bifrost_rename", || {
        clear_error();
        let Some(name) = (unsafe { cstr(name) }) else {
            set_error("bifrost_rename needs a name");
            return -1;
        };
        rename(&name)
    })
}

// ─── Voice ───────────────────────────────────────────────────────────────────

/// Place a call. Returns the session id, or `NULL` on failure. Caller frees.
///
/// # Safety
/// `peer` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn bifrost_call(peer: *const c_char) -> *mut c_char {
    guard_string("bifrost_call", || {
        clear_error();
        let Some(peer) = (unsafe { cstr(peer) }) else {
            set_error("bifrost_call needs a peer");
            return std::ptr::null_mut();
        };

        match start_call(&peer) {
            Some(session_id) => out_string(session_id),
            None => std::ptr::null_mut(),
        }
    })
}

/// End a call. `session` may be `NULL` to end the only active one.
///
/// # Safety
/// `session` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn bifrost_hangup(session: *const c_char) -> c_int {
    guard_int("bifrost_hangup", || {
        clear_error();
        hang_up(unsafe { cstr(session) }.as_deref())
    })
}

/// Send one captured audio frame: signed 16-bit little-endian mono at 16 kHz.
///
/// Call roughly every 20 ms with 320 samples (640 bytes) from the platform
/// microphone.
///
/// # Safety
/// `session` must be a valid NUL-terminated C string, and `pcm` must point to
/// at least `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn bifrost_voice_send_frame(
    session: *const c_char,
    pcm: *const u8,
    len: usize,
) -> c_int {
    guard_int("bifrost_voice_send_frame", || {
        let Some(session) = (unsafe { cstr(session) }) else {
            set_error("bifrost_voice_send_frame needs a session id");
            return -1;
        };
        if pcm.is_null() || len == 0 {
            set_error("bifrost_voice_send_frame needs audio data");
            return -1;
        }

        send_audio_frame(&session, unsafe { std::slice::from_raw_parts(pcm, len) })
    })
}

/// Drain received audio for a session as a JSON array of base64 frames.
///
/// Base64 rather than raw bytes because the return type is a C string, and
/// PCM contains NUL bytes that would truncate it. Caller frees.
///
/// # Safety
/// `session` must be a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn bifrost_voice_take_frames(session: *const c_char) -> *mut c_char {
    guard_string("bifrost_voice_take_frames", || {
        let Some(session) = (unsafe { cstr(session) }) else {
            set_error("bifrost_voice_take_frames needs a session id");
            return std::ptr::null_mut();
        };
        out_string(take_audio_json(&session))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Read and free a string the library returned.
    fn take(ptr: *mut c_char) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        let value = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { bifrost_string_free(ptr) };
        Some(value)
    }

    /// These tests share one process-global node, so they run as one test to
    /// keep cargo's threads from interleaving start and stop.
    #[test]
    fn the_ffi_surface_works_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        // Bind well away from the default ports so a real node on this machine
        // is not disturbed.
        let mut config = Config::generate();
        config.ports = crate::net::protocol::PortSet::default().offset_by(300);
        // Shift discovery too, so this test node is invisible to the other
        // tests and to any real node on the developer's network.
        config.ports.discovery += 300;
        config.download_dir = dir.path().join("downloads");
        config.data_dir = dir.path().join("data");
        config.save(&config_path).unwrap();

        // Calls before start must report, not crash.
        assert_eq!(bifrost_is_running(), 0);
        assert!(bifrost_snapshot_json().is_null());
        assert!(take(bifrost_last_error())
            .unwrap()
            .contains("bifrost_start"));

        let path = CString::new(config_path.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { bifrost_start(path.as_ptr()) },
            0,
            "start should succeed"
        );
        assert_eq!(bifrost_is_running(), 1);
        // Starting twice is a no-op, not a failure.
        assert_eq!(unsafe { bifrost_start(path.as_ptr()) }, 1);

        let node_id = take(bifrost_node_id()).expect("node id");
        assert!(!node_id.is_empty());

        // The snapshot is valid JSON with the fields a client renders.
        let snapshot: serde_json::Value =
            serde_json::from_str(&take(bifrost_snapshot_json()).unwrap()).unwrap();
        assert_eq!(snapshot["identity"]["node_id"], node_id.as_str());
        assert!(snapshot["telemetry"]["ram_total_mb"].as_u64().unwrap() > 0);
        assert!(snapshot["ports"]["messaging"].as_u64().unwrap() == 9302);

        // Arrays come back as arrays even when empty.
        for json in [
            bifrost_peers_json(),
            bifrost_transfers_json(),
            bifrost_calls_json(),
        ] {
            let value: serde_json::Value = serde_json::from_str(&take(json).unwrap()).unwrap();
            assert!(value.is_array());
        }

        // A broadcast lands in history.
        let message = CString::new("hello from the FFI").unwrap();
        assert_eq!(
            unsafe { bifrost_send_chat(message.as_ptr(), std::ptr::null()) },
            0
        );
        let chat: serde_json::Value =
            serde_json::from_str(&take(bifrost_chat_json(10)).unwrap()).unwrap();
        assert_eq!(chat[0]["content"], "hello from the FFI");

        // The command path mirrors the terminal dashboard.
        let line = CString::new("/help").unwrap();
        let outcome: serde_json::Value =
            serde_json::from_str(&take(unsafe { bifrost_execute(line.as_ptr()) }).unwrap())
                .unwrap();
        assert_eq!(outcome["outcome"], "message");

        // An empty line is a no-op.
        let blank = CString::new("   ").unwrap();
        assert_eq!(
            take(unsafe { bifrost_execute(blank.as_ptr()) }).unwrap(),
            "null"
        );

        // Draining is destructive: an event delivered once is not delivered
        // again. Asserting the second drain is *empty* would be a race, since
        // the node keeps publishing telemetry every couple of seconds, so
        // instead track one specific event across two drains.
        let _ = take(bifrost_poll_events());
        let marker = CString::new("ffi-drain-marker").unwrap();
        assert_eq!(
            unsafe { bifrost_send_chat(marker.as_ptr(), std::ptr::null()) },
            0
        );

        let contains_marker = |json: &str| -> bool {
            serde_json::from_str::<serde_json::Value>(json)
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["data"]["content"] == "ffi-drain-marker")
        };

        // The buffer is filled by a background task draining the event bus, so
        // the event has not necessarily landed the instant send_chat returns.
        let mut delivered = false;
        for _ in 0..40 {
            if contains_marker(&take(bifrost_poll_events()).unwrap()) {
                delivered = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            delivered,
            "the marker event should arrive within two seconds"
        );

        // Having been delivered, it must never come back.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let second = take(bifrost_poll_events()).unwrap();
        assert!(
            !contains_marker(&second),
            "the second drain must not repeat it"
        );

        // Sharing toggles without error.
        assert_eq!(bifrost_set_sharing(0), 0);
        assert_eq!(bifrost_set_sharing(1), 0);

        // A rename is reflected in the next snapshot.
        let name = CString::new("ffi-test-node").unwrap();
        assert_eq!(unsafe { bifrost_rename(name.as_ptr()) }, 0);
        let snapshot: serde_json::Value =
            serde_json::from_str(&take(bifrost_snapshot_json()).unwrap()).unwrap();
        assert_eq!(snapshot["identity"]["username"], "ffi-test-node");

        // Null and invalid arguments are refused rather than crashing.
        assert_eq!(
            unsafe { bifrost_send_chat(std::ptr::null(), std::ptr::null()) },
            -1
        );
        assert_eq!(
            unsafe { bifrost_send_file(std::ptr::null(), std::ptr::null()) },
            -1
        );
        assert!(unsafe { bifrost_execute(std::ptr::null()) }.is_null());
        assert!(unsafe { bifrost_call(std::ptr::null()) }.is_null());

        // Actions against an unknown peer fail cleanly.
        let ghost = CString::new("no-such-peer").unwrap();
        assert!(unsafe { bifrost_call(ghost.as_ptr()) }.is_null());
        let frame = vec![0u8; 640];
        let session = CString::new("no-such-session").unwrap();
        assert_eq!(
            unsafe { bifrost_voice_send_frame(session.as_ptr(), frame.as_ptr(), frame.len()) },
            -1
        );

        // Version metadata is available without a running node.
        assert_eq!(take(bifrost_version()).unwrap(), crate::VERSION);
        assert_eq!(bifrost_protocol_version(), crate::PROTOCOL_VERSION as c_int);

        assert_eq!(bifrost_stop(), 0);
        assert_eq!(bifrost_is_running(), 0);
        // Stopping twice is harmless.
        assert_eq!(bifrost_stop(), 0);

        // Freeing null is a no-op, as C callers expect.
        unsafe { bifrost_string_free(std::ptr::null_mut()) };
    }
}
