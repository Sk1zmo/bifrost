// Raw `dart:ffi` bindings to libbifrost_core.
//
// This file mirrors the C ABI in `crates/core/src/ffi.rs` one function at a
// time and does nothing else. Anything higher-level belongs in
// `bifrost_client.dart`, so that when the Rust signature changes there is
// exactly one place to update.
//
// Memory rule: every `Pointer<Utf8>` the library returns is owned by us and
// must be released with `bifrost_string_free`. `_takeString` does that, and is
// the only sanctioned way to read a returned string.

import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

// ─── Native signatures ───────────────────────────────────────────────────────

typedef _StartNative = Int32 Function(Pointer<Utf8>);
typedef _StartDart = int Function(Pointer<Utf8>);

typedef _VoidToIntNative = Int32 Function();
typedef _VoidToIntDart = int Function();

typedef _VoidToStringNative = Pointer<Utf8> Function();
typedef _VoidToStringDart = Pointer<Utf8> Function();

typedef _StringToIntNative = Int32 Function(Pointer<Utf8>);
typedef _StringToIntDart = int Function(Pointer<Utf8>);

typedef _StringToStringNative = Pointer<Utf8> Function(Pointer<Utf8>);
typedef _StringToStringDart = Pointer<Utf8> Function(Pointer<Utf8>);

typedef _TwoStringsToIntNative = Int32 Function(Pointer<Utf8>, Pointer<Utf8>);
typedef _TwoStringsToIntDart = int Function(Pointer<Utf8>, Pointer<Utf8>);

typedef _IntToIntNative = Int32 Function(Int32);
typedef _IntToIntDart = int Function(int);

typedef _ChatJsonNative = Pointer<Utf8> Function(Int32);
typedef _ChatJsonDart = Pointer<Utf8> Function(int);

typedef _FreeNative = Void Function(Pointer<Utf8>);
typedef _FreeDart = void Function(Pointer<Utf8>);

typedef _SendFrameNative = Int32 Function(Pointer<Utf8>, Pointer<Uint8>, IntPtr);
typedef _SendFrameDart = int Function(Pointer<Utf8>, Pointer<Uint8>, int);

/// Thrown when the native library reports a failure.
class BifrostNativeError implements Exception {
  BifrostNativeError(this.message);
  final String message;
  @override
  String toString() => 'Bifrost: $message';
}

/// Thin wrapper over the loaded shared library.
class BifrostFfi {
  BifrostFfi._(this._library) {
    _start = _library.lookupFunction<_StartNative, _StartDart>('bifrost_start');
    _stop = _library.lookupFunction<_VoidToIntNative, _VoidToIntDart>('bifrost_stop');
    _isRunning =
        _library.lookupFunction<_VoidToIntNative, _VoidToIntDart>('bifrost_is_running');
    _lastError = _library
        .lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_last_error');
    _version =
        _library.lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_version');
    _protocolVersion = _library
        .lookupFunction<_VoidToIntNative, _VoidToIntDart>('bifrost_protocol_version');
    _free = _library.lookupFunction<_FreeNative, _FreeDart>('bifrost_string_free');

    _nodeId =
        _library.lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_node_id');
    _snapshot = _library
        .lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_snapshot_json');
    _peers = _library
        .lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_peers_json');
    _chat = _library.lookupFunction<_ChatJsonNative, _ChatJsonDart>('bifrost_chat_json');
    _transfers = _library
        .lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_transfers_json');
    _calls = _library
        .lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_calls_json');
    _pollEvents = _library
        .lookupFunction<_VoidToStringNative, _VoidToStringDart>('bifrost_poll_events');

    _execute = _library
        .lookupFunction<_StringToStringNative, _StringToStringDart>('bifrost_execute');
    _sendChat = _library
        .lookupFunction<_TwoStringsToIntNative, _TwoStringsToIntDart>('bifrost_send_chat');
    _sendFile = _library
        .lookupFunction<_TwoStringsToIntNative, _TwoStringsToIntDart>('bifrost_send_file');
    _setAlias = _library
        .lookupFunction<_TwoStringsToIntNative, _TwoStringsToIntDart>('bifrost_set_alias');
    _setSharing =
        _library.lookupFunction<_IntToIntNative, _IntToIntDart>('bifrost_set_sharing');
    _rename =
        _library.lookupFunction<_StringToIntNative, _StringToIntDart>('bifrost_rename');

    _call =
        _library.lookupFunction<_StringToStringNative, _StringToStringDart>('bifrost_call');
    _hangup =
        _library.lookupFunction<_StringToIntNative, _StringToIntDart>('bifrost_hangup');
    _voiceSend = _library
        .lookupFunction<_SendFrameNative, _SendFrameDart>('bifrost_voice_send_frame');
    _voiceTake = _library.lookupFunction<_StringToStringNative, _StringToStringDart>(
        'bifrost_voice_take_frames');
  }

  /// Open the library for this platform.
  ///
  /// iOS and macOS link the static library into the app binary, so symbols
  /// live in the process itself and there is nothing to open.
  factory BifrostFfi.open() {
    final DynamicLibrary library;
    if (Platform.isAndroid) {
      library = DynamicLibrary.open('libbifrost_core.so');
    } else if (Platform.isIOS || Platform.isMacOS) {
      library = DynamicLibrary.process();
    } else if (Platform.isWindows) {
      library = DynamicLibrary.open('bifrost_core.dll');
    } else if (Platform.isLinux) {
      library = DynamicLibrary.open('libbifrost_core.so');
    } else {
      throw UnsupportedError('Bifrost has no native library for this platform');
    }
    return BifrostFfi._(library);
  }

  final DynamicLibrary _library;

  late final _StartDart _start;
  late final _VoidToIntDart _stop;
  late final _VoidToIntDart _isRunning;
  late final _VoidToStringDart _lastError;
  late final _VoidToStringDart _version;
  late final _VoidToIntDart _protocolVersion;
  late final _FreeDart _free;
  late final _VoidToStringDart _nodeId;
  late final _VoidToStringDart _snapshot;
  late final _VoidToStringDart _peers;
  late final _ChatJsonDart _chat;
  late final _VoidToStringDart _transfers;
  late final _VoidToStringDart _calls;
  late final _VoidToStringDart _pollEvents;
  late final _StringToStringDart _execute;
  late final _TwoStringsToIntDart _sendChat;
  late final _TwoStringsToIntDart _sendFile;
  late final _TwoStringsToIntDart _setAlias;
  late final _IntToIntDart _setSharing;
  late final _StringToIntDart _rename;
  late final _StringToStringDart _call;
  late final _StringToIntDart _hangup;
  late final _SendFrameDart _voiceSend;
  late final _StringToStringDart _voiceTake;

  // ── Helpers ────────────────────────────────────────────────────────────────

  /// Read a returned string and free the native allocation.
  String? _takeString(Pointer<Utf8> pointer) {
    if (pointer == nullptr) return null;
    try {
      return pointer.toDartString();
    } finally {
      // Freed even if decoding throws, so a malformed reply cannot leak.
      _free(pointer);
    }
  }

  /// Run `action` with a temporary C string, always freeing it afterwards.
  T _withCString<T>(String? value, T Function(Pointer<Utf8>) action) {
    if (value == null) return action(nullptr);
    final pointer = value.toNativeUtf8();
    try {
      return action(pointer);
    } finally {
      calloc.free(pointer);
    }
  }

  /// Turn a `-1` status into an exception carrying the native message.
  void _check(int status, String what) {
    if (status < 0) {
      throw BifrostNativeError('$what failed: ${lastError() ?? "unknown error"}');
    }
  }

  // ── Lifecycle ──────────────────────────────────────────────────────────────

  /// Start the node.
  ///
  /// Blocks for a moment while services bind. Callers should run it off the UI
  /// thread — see `BifrostClient.start`.
  void start(String configPath) {
    final status = _withCString(configPath, _start);
    // 1 means "already running", which is success from the caller's point of
    // view: hot restart in debug calls this twice.
    if (status < 0) {
      throw BifrostNativeError('could not start: ${lastError() ?? "unknown error"}');
    }
  }

  void stop() => _check(_stop(), 'stop');

  bool get isRunning => _isRunning() != 0;

  String? lastError() {
    final message = _takeString(_lastError());
    return (message == null || message.isEmpty) ? null : message;
  }

  String get version => _takeString(_version()) ?? 'unknown';

  int get protocolVersion => _protocolVersion();

  // ── Reading state ──────────────────────────────────────────────────────────

  String? get nodeId => _takeString(_nodeId());
  String? snapshotJson() => _takeString(_snapshot());
  String? peersJson() => _takeString(_peers());
  String? chatJson({int limit = 200}) => _takeString(_chat(limit));
  String? transfersJson() => _takeString(_transfers());
  String? callsJson() => _takeString(_calls());
  String? pollEventsJson() => _takeString(_pollEvents());

  // ── Actions ────────────────────────────────────────────────────────────────

  /// Run a command line, returning the outcome as JSON.
  String? execute(String line) =>
      _withCString(line, (pointer) => _takeString(_execute(pointer)));

  void sendChat(String content, {String? recipient}) {
    final status = _withCString(content, (c) => _withCString(recipient, (r) => _sendChat(c, r)));
    _check(status, 'sending the message');
  }

  void sendFile(String peer, String path) {
    final status = _withCString(peer, (p) => _withCString(path, (f) => _sendFile(p, f)));
    _check(status, 'sending the file');
  }

  void setAlias(String peer, String? alias) {
    final status = _withCString(peer, (p) => _withCString(alias, (a) => _setAlias(p, a)));
    _check(status, 'setting the alias');
  }

  void setSharing(bool enabled) => _check(_setSharing(enabled ? 1 : 0), 'toggling sharing');

  void rename(String name) =>
      _check(_withCString(name, _rename), 'renaming this device');

  // ── Voice ──────────────────────────────────────────────────────────────────

  /// Start a call, returning the session id.
  String startCall(String peer) {
    final session = _withCString(peer, (p) => _takeString(_call(p)));
    if (session == null) {
      throw BifrostNativeError('could not call: ${lastError() ?? "unknown error"}');
    }
    return session;
  }

  void hangUp({String? session}) =>
      _check(_withCString(session, _hangup), 'ending the call');

  /// Send one 20 ms frame of signed 16-bit little-endian mono PCM at 16 kHz.
  void sendAudioFrame(String session, Uint8List pcm) {
    // Copy into native memory: Dart's GC may move a typed-data buffer, so its
    // address is not stable across a native call.
    final buffer = calloc<Uint8>(pcm.length);
    try {
      buffer.asTypedList(pcm.length).setAll(0, pcm);
      final status = _withCString(session, (s) => _voiceSend(s, buffer, pcm.length));
      _check(status, 'sending audio');
    } finally {
      calloc.free(buffer);
    }
  }

  /// Drain received audio as a JSON array of base64 frames.
  String? takeAudioFrames(String session) =>
      _withCString(session, (s) => _takeString(_voiceTake(s)));
}
