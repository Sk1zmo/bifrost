// App-wide state, exposed to the widget tree through Provider.

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../bridge/bifrost_client.dart';
import '../models/models.dart';

/// One line in the activity log.
class LogLine {
  LogLine(this.text, this.level) : at = DateTime.now();
  final String text;
  final String level;
  final DateTime at;
}

class NodeState extends ChangeNotifier {
  BifrostClient? _client;
  StreamSubscription<NodeSnapshot>? _snapshots;
  StreamSubscription<NodeEvent>? _events;

  NodeSnapshot? _snapshot;
  final List<LogLine> _log = [];
  String? _startupError;
  bool _starting = true;

  NodeSnapshot? get snapshot => _snapshot;
  List<LogLine> get log => List.unmodifiable(_log);
  String? get startupError => _startupError;
  bool get starting => _starting;
  bool get ready => _client != null && _snapshot != null;

  List<Peer> get peers => _snapshot?.peers ?? const [];
  List<Peer> get onlinePeers => _snapshot?.onlinePeerList ?? const [];
  List<ChatMessage> get chats => _snapshot?.chats ?? const [];
  List<FileTransfer> get transfers => _snapshot?.transfers ?? const [];
  List<CallSession> get calls => _snapshot?.calls ?? const [];
  Telemetry get telemetry => _snapshot?.telemetry ?? Telemetry.empty;
  PoolTotals get pool => _snapshot?.pool ?? PoolTotals.empty;

  CallSession? get activeCall {
    for (final call in calls) {
      if (call.isConnected) return call;
    }
    return null;
  }

  /// Start the node and subscribe to it.
  Future<void> initialise() async {
    try {
      final client = await BifrostClient.start();
      _client = client;

      _snapshots = client.snapshots.listen((snapshot) {
        _snapshot = snapshot;
        notifyListeners();
      });

      _events = client.events.listen((event) {
        if (event.isNotice) {
          _push(event.text, event.level);
        } else if (event.isChat) {
          final message = event.chat;
          if (message != null && message.senderId != _snapshot?.nodeId) {
            _push('${message.senderName}: ${message.content}', 'info');
          }
        }
      });

      // Draw something immediately rather than waiting for the first tick.
      _snapshot = client.snapshot();
      _push('Bifrost started', 'success');
    } catch (e) {
      _startupError = e.toString();
    } finally {
      _starting = false;
      notifyListeners();
    }
  }

  void _push(String text, String level) {
    _log.insert(0, LogLine(text, level));
    if (_log.length > 200) _log.removeRange(200, _log.length);
    notifyListeners();
  }

  /// Run an action, surfacing any failure in the log instead of throwing into
  /// a widget callback where it would become an unhandled error.
  void _guard(String what, void Function(BifrostClient) action) {
    final client = _client;
    if (client == null) {
      _push('Not connected yet', 'warning');
      return;
    }
    try {
      action(client);
    } catch (e) {
      _push('$what failed: $e', 'error');
    }
  }

  void broadcast(String message) {
    if (message.trim().isEmpty) return;
    _guard('Sending', (c) => c.broadcast(message.trim()));
    _refresh();
  }

  void directMessage(String peerId, String message) {
    if (message.trim().isEmpty) return;
    _guard('Sending', (c) => c.directMessage(peerId, message.trim()));
    _refresh();
  }

  void sendFile(String peerId, String path) {
    _guard('Sending the file', (c) => c.sendFile(peerId, path));
    _refresh();
  }

  void setAlias(String peerId, String? alias) {
    _guard('Renaming', (c) => c.setAlias(peerId, alias));
    _refresh();
  }

  void setSharing(bool enabled) {
    _guard('Toggling sharing', (c) => c.setSharing(enabled));
    _refresh();
  }

  void rename(String name) {
    _guard('Renaming this device', (c) => c.rename(name));
    _refresh();
  }

  void startCall(String peerId) {
    _guard('Calling', (c) {
      final session = c.startCall(peerId);
      _push('In a call (session ${session.substring(0, 8)})', 'success');
    });
    _refresh();
  }

  void hangUp({String? session}) {
    _guard('Ending the call', (c) => c.hangUp(session: session));
    _refresh();
  }

  /// Run a raw command line and log its reply.
  void runCommand(String line) {
    _guard('Command', (c) {
      final reply = c.executeLine(line);
      if (reply.isNotEmpty) _push(reply, 'info');
    });
    _refresh();
  }

  /// Pull a snapshot now, so the UI reacts to an action immediately rather
  /// than at the next poll.
  void _refresh() {
    final snapshot = _client?.snapshot();
    if (snapshot != null) {
      _snapshot = snapshot;
      notifyListeners();
    }
  }

  @override
  void dispose() {
    _snapshots?.cancel();
    _events?.cancel();
    _client?.dispose();
    super.dispose();
  }
}
