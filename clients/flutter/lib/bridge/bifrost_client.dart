// High-level client over the raw FFI bindings.
//
// Turns the JSON-in-C-strings surface into Dart models, keeps the node's
// lifecycle tied to the app's, and does the blocking work off the UI isolate.

import 'dart:async';
import 'dart:convert';
import 'dart:isolate';
import 'dart:typed_data';

import 'package:path_provider/path_provider.dart';

import '../models/models.dart';
import 'bifrost_ffi.dart';

/// Runs `bifrost_start` in a background isolate.
///
/// It blocks for a moment while sockets bind, which would drop frames on the
/// UI isolate. The Rust side keeps its node in a process-global, so starting
/// it from another isolate still leaves it visible to every later call — which
/// is exactly why the FFI is shaped that way.
void _startInIsolate(String configPath) {
  BifrostFfi.open().start(configPath);
}

/// The app's single connection to the Rust core.
class BifrostClient {
  BifrostClient._(this._ffi);

  static BifrostClient? _instance;
  final BifrostFfi _ffi;

  Timer? _poller;
  final _events = StreamController<NodeEvent>.broadcast();
  final _snapshots = StreamController<NodeSnapshot>.broadcast();

  /// Live node events: chat, transfers, peers coming and going.
  Stream<NodeEvent> get events => _events.stream;

  /// A fresh snapshot on every poll tick.
  Stream<NodeSnapshot> get snapshots => _snapshots.stream;

  String get version => _ffi.version;
  bool get isRunning => _ffi.isRunning;

  /// Start the node and begin polling.
  ///
  /// The config goes in the app's documents directory: on both Android and
  /// iOS the working directory is not writable, so the Rust default path
  /// would fail.
  static Future<BifrostClient> start() async {
    if (_instance != null) return _instance!;

    final documents = await getApplicationDocumentsDirectory();
    final configPath = '${documents.path}/bifrost/config.json';

    await Isolate.run(() => _startInIsolate(configPath));

    final client = BifrostClient._(BifrostFfi.open());
    client._beginPolling();
    _instance = client;
    return client;
  }

  /// Stop the node. Call from the app's lifecycle handler.
  Future<void> stop() async {
    _poller?.cancel();
    _poller = null;
    _ffi.stop();
    _instance = null;
  }

  void dispose() {
    _poller?.cancel();
    _events.close();
    _snapshots.close();
  }

  /// Poll for events and a snapshot on one timer.
  ///
  /// The Rust side buffers events rather than calling back into Dart: a
  /// callback from a Rust thread would need an isolate port and a live run
  /// loop, and would be one more thing to get wrong on two platforms.
  void _beginPolling() {
    _poller = Timer.periodic(const Duration(milliseconds: 900), (_) {
      final rawEvents = _ffi.pollEventsJson();
      if (rawEvents != null) {
        for (final item in jsonDecode(rawEvents) as List<dynamic>) {
          _events.add(NodeEvent.fromJson(item as Map<String, dynamic>));
        }
      }

      final rawSnapshot = _ffi.snapshotJson();
      if (rawSnapshot != null) {
        _snapshots.add(
          NodeSnapshot.fromJson(jsonDecode(rawSnapshot) as Map<String, dynamic>),
        );
      }
    });
  }

  /// A snapshot right now, without waiting for the next tick.
  NodeSnapshot? snapshot() {
    final raw = _ffi.snapshotJson();
    if (raw == null) return null;
    return NodeSnapshot.fromJson(jsonDecode(raw) as Map<String, dynamic>);
  }

  List<Peer> peers() {
    final raw = _ffi.peersJson();
    if (raw == null) return const [];
    return (jsonDecode(raw) as List<dynamic>)
        .map((p) => Peer.fromJson(p as Map<String, dynamic>))
        .toList();
  }

  List<ChatMessage> chat({int limit = 200}) {
    final raw = _ffi.chatJson(limit: limit);
    if (raw == null) return const [];
    return (jsonDecode(raw) as List<dynamic>)
        .map((m) => ChatMessage.fromJson(m as Map<String, dynamic>))
        .toList();
  }

  List<FileTransfer> transfers() {
    final raw = _ffi.transfersJson();
    if (raw == null) return const [];
    return (jsonDecode(raw) as List<dynamic>)
        .map((t) => FileTransfer.fromJson(t as Map<String, dynamic>))
        .toList();
  }

  List<CallSession> calls() {
    final raw = _ffi.callsJson();
    if (raw == null) return const [];
    return (jsonDecode(raw) as List<dynamic>)
        .map((c) => CallSession.fromJson(c as Map<String, dynamic>))
        .toList();
  }

  // ── Actions ────────────────────────────────────────────────────────────────

  void broadcast(String message) => _ffi.sendChat(message);

  void directMessage(String peerId, String message) =>
      _ffi.sendChat(message, recipient: peerId);

  void sendFile(String peerId, String path) => _ffi.sendFile(peerId, path);

  void setAlias(String peerId, String? alias) => _ffi.setAlias(peerId, alias);

  void setSharing(bool enabled) => _ffi.setSharing(enabled);

  void rename(String name) => _ffi.rename(name);

  String startCall(String peerId) => _ffi.startCall(peerId);

  void hangUp({String? session}) => _ffi.hangUp(session: session);

  void sendAudioFrame(String session, Uint8List pcm) =>
      _ffi.sendAudioFrame(session, pcm);

  /// Drain received audio frames for a call.
  List<Uint8List> takeAudioFrames(String session) {
    final raw = _ffi.takeAudioFrames(session);
    if (raw == null) return const [];
    return (jsonDecode(raw) as List<dynamic>)
        .map((frame) => base64Decode(frame as String))
        .toList();
  }

  /// Run a raw command line, exactly as the terminal dashboard would.
  ///
  /// Everything the phone UI does has a typed method above; this exists so a
  /// power user can type `/status` and get the same answer as on a laptop.
  String executeLine(String line) {
    final raw = _ffi.execute(line);
    if (raw == null || raw == 'null') return '';
    final outcome = jsonDecode(raw);
    if (outcome is Map<String, dynamic>) {
      return outcome['text'] as String? ?? outcome['outcome']?.toString() ?? '';
    }
    return '';
  }
}
