//! JNI entry points for the Android client.
//!
//! Android cannot call the C ABI in [`crate::ffi`] directly: JNI resolves a
//! native method by a name derived from its package and class, so each one
//! needs a `Java_dev_bifrost_app_BifrostNative_*` symbol. This module is that
//! bridge and nothing more — every function converts its arguments, calls the
//! same code the desktop and iOS use, and converts the result back.
//!
//! Compiled only for Android, so no other target pays for the `jni` crate.
//!
//! ## Rules
//!
//! * **No panic may escape.** Unwinding into the JVM is undefined behaviour,
//!   so every entry point catches and returns a failure value instead.
//! * **Errors are returned, never thrown.** Throwing a Java exception from
//!   Rust is possible but easy to get wrong; a status code plus
//!   [`last_error`](crate::ffi::bifrost_last_error) is simpler to keep correct.
//! * **Logging goes to logcat**, because Android discards stderr.

#![allow(non_snake_case)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jbyteArray, jint, jstring};
use jni::JNIEnv;

use crate::node::Node;
use crate::util::config::Config;

// ─── Conversions ─────────────────────────────────────────────────────────────

/// Read a Java string, treating null as `None`.
fn take_string(env: &mut JNIEnv, value: &JString) -> Option<String> {
    if value.is_null() {
        return None;
    }
    env.get_string(value).ok().map(|s| s.into())
}

/// Hand a Rust string back to Java. Returns null on failure, which every
/// caller on the Kotlin side already treats as "no result".
fn out_string(env: &JNIEnv, value: &str) -> jstring {
    match env.new_string(value) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn guard_string(env: &JNIEnv, name: &str, f: impl FnOnce() -> String) -> jstring {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => out_string(env, &value),
        Err(_) => {
            crate::ffi::record_error(format!("{name} panicked"));
            std::ptr::null_mut()
        }
    }
}

fn guard_int(name: &str, f: impl FnOnce() -> jint) -> jint {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(code) => code,
        Err(_) => {
            crate::ffi::record_error(format!("{name} panicked"));
            -1
        }
    }
}

// ─── Logging ─────────────────────────────────────────────────────────────────

/// Route tracing output to logcat.
///
/// Android throws away stderr, so without this a node that failed to start
/// would do so completely silently — the single most painful way to debug a
/// phone.
mod logcat {
    use std::ffi::CString;
    use std::io::Write;

    // Provided by liblog, which Android links into every process.
    extern "C" {
        fn __android_log_write(
            prio: i32,
            tag: *const std::ffi::c_char,
            text: *const std::ffi::c_char,
        ) -> i32;
    }

    const INFO: i32 = 4;
    const TAG: &str = "Bifrost";

    pub struct LogcatWriter;

    impl Write for LogcatWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let len = buf.len();
            // Interior NULs and invalid UTF-8 are possible in principle; drop
            // the line rather than failing the write, since losing a log line
            // must never break the thing being logged.
            if let Ok(text) = std::str::from_utf8(buf) {
                if let (Ok(tag), Ok(message)) = (CString::new(TAG), CString::new(text.trim_end())) {
                    unsafe { __android_log_write(INFO, tag.as_ptr(), message.as_ptr()) };
                }
            }
            Ok(len)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogcatWriter {
        type Writer = LogcatWriter;
        fn make_writer(&'a self) -> Self::Writer {
            LogcatWriter
        }
    }
}

fn init_logging() {
    use std::sync::Once;
    static ONCE: Once = Once::new();

    ONCE.call_once(|| {
        let filter = std::env::var("BIFROST_LOG")
            .ok()
            .and_then(|raw| tracing_subscriber::EnvFilter::try_new(raw).ok())
            .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));

        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(logcat::LogcatWriter)
            .try_init();
    });
}

// ─── Lifecycle ───────────────────────────────────────────────────────────────

/// Start the node.
///
/// `configPath` must be inside the app's own storage: Android gives an app no
/// writable working directory, so the default path would fail.
///
/// Returns 0 on success, 1 if already running, -1 on failure.
#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    config_path: JString,
) -> jint {
    init_logging();

    let path = take_string(&mut env, &config_path);
    guard_int("nativeStart", || {
        let Some(path) = path else {
            crate::ffi::record_error("a config path is required on Android");
            return -1;
        };

        if crate::ffi::node_is_running() {
            return 1;
        }

        let config = match Config::load_or_create(&PathBuf::from(&path)) {
            Ok(config) => config,
            Err(e) => {
                crate::ffi::record_error(format!("could not load the config: {e:#}"));
                return -1;
            }
        };

        match crate::ffi::runtime().block_on(Node::start(config, &PathBuf::from(&path))) {
            Ok(node) => {
                crate::ffi::install_node(node);
                0
            }
            Err(e) => {
                crate::ffi::record_error(format!("could not start the node: {e:#}"));
                -1
            }
        }
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeStop(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    guard_int("nativeStop", || {
        crate::ffi::shutdown_node();
        0
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeIsRunning(
    _env: JNIEnv,
    _class: JClass,
) -> jint {
    guard_int("nativeIsRunning", || {
        jint::from(crate::ffi::node_is_running())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeLastError(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    guard_string(&env, "nativeLastError", crate::ffi::last_error_text)
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeVersion(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    guard_string(&env, "nativeVersion", || crate::VERSION.to_string())
}

// ─── Reading state ───────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeSnapshotJson(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    guard_string(&env, "nativeSnapshotJson", || {
        crate::ffi::with_node_json(|node| crate::ffi::runtime().block_on(node.snapshot()))
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativePollEventsJson(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    guard_string(&env, "nativePollEventsJson", crate::ffi::drain_events_json)
}

// ─── Actions ─────────────────────────────────────────────────────────────────

/// Run a command line, exactly as the terminal dashboard would.
#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeExecute(
    mut env: JNIEnv,
    _class: JClass,
    line: JString,
) -> jstring {
    let line = take_string(&mut env, &line).unwrap_or_default();
    guard_string(&env, "nativeExecute", || {
        crate::ffi::execute_line_json(&line)
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeSendChat(
    mut env: JNIEnv,
    _class: JClass,
    content: JString,
    recipient: JString,
) -> jint {
    let content = take_string(&mut env, &content);
    let recipient = take_string(&mut env, &recipient);

    guard_int("nativeSendChat", || {
        let Some(content) = content else { return -1 };
        crate::ffi::send_chat(&content, recipient.as_deref())
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeSendFile(
    mut env: JNIEnv,
    _class: JClass,
    peer: JString,
    path: JString,
) -> jint {
    let peer = take_string(&mut env, &peer);
    let path = take_string(&mut env, &path);

    guard_int("nativeSendFile", || match (peer, path) {
        (Some(peer), Some(path)) => crate::ffi::send_file(&peer, &path),
        _ => -1,
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeSetAlias(
    mut env: JNIEnv,
    _class: JClass,
    peer: JString,
    alias: JString,
) -> jint {
    let peer = take_string(&mut env, &peer);
    let alias = take_string(&mut env, &alias);

    guard_int("nativeSetAlias", || match peer {
        Some(peer) => crate::ffi::set_alias(&peer, alias),
        None => -1,
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeSetSharing(
    _env: JNIEnv,
    _class: JClass,
    enabled: jint,
) -> jint {
    guard_int("nativeSetSharing", || crate::ffi::set_sharing(enabled != 0))
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeRename(
    mut env: JNIEnv,
    _class: JClass,
    name: JString,
) -> jint {
    let name = take_string(&mut env, &name);
    guard_int("nativeRename", || match name {
        Some(name) => crate::ffi::rename(&name),
        None => -1,
    })
}

// ─── Voice ───────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeCall(
    mut env: JNIEnv,
    _class: JClass,
    peer: JString,
) -> jstring {
    let peer = take_string(&mut env, &peer).unwrap_or_default();
    guard_string(&env, "nativeCall", || {
        crate::ffi::start_call(&peer).unwrap_or_default()
    })
}

#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeHangUp(
    mut env: JNIEnv,
    _class: JClass,
    session: JString,
) -> jint {
    let session = take_string(&mut env, &session);
    guard_int("nativeHangUp", || crate::ffi::hang_up(session.as_deref()))
}

/// Send one captured audio frame: signed 16-bit little-endian mono at 16 kHz.
#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeSendAudio(
    mut env: JNIEnv,
    _class: JClass,
    session: JString,
    pcm: jbyteArray,
) -> jint {
    let session = take_string(&mut env, &session);

    // Copy out of the JVM heap before doing anything async with it.
    let frame = {
        let array = unsafe { JObject::from_raw(pcm) };
        match env
            .convert_byte_array(unsafe { jni::objects::JByteArray::from_raw(array.into_raw()) })
        {
            Ok(bytes) => bytes,
            Err(_) => return -1,
        }
    };

    guard_int("nativeSendAudio", || match session {
        Some(session) => crate::ffi::send_audio_frame(&session, &frame),
        None => -1,
    })
}

/// Drain received audio for a call, as a JSON array of base64 frames.
#[no_mangle]
pub extern "system" fn Java_dev_bifrost_app_BifrostNative_nativeTakeAudio(
    mut env: JNIEnv,
    _class: JClass,
    session: JString,
) -> jstring {
    let session = take_string(&mut env, &session).unwrap_or_default();
    guard_string(&env, "nativeTakeAudio", || {
        crate::ffi::take_audio_json(&session)
    })
}
