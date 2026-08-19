// Dart mirrors of the JSON the Rust core produces.
//
// Field names match `serde`'s output exactly. Every parser tolerates a missing
// or null field, so a phone running an older build of the app against a newer
// node degrades rather than crashing.

import 'dart:convert';

double _asDouble(dynamic v) => (v as num?)?.toDouble() ?? 0.0;
int _asInt(dynamic v) => (v as num?)?.toInt() ?? 0;

class Telemetry {
  const Telemetry({
    required this.cpuUsage,
    required this.cpuCores,
    required this.ramUsedMb,
    required this.ramTotalMb,
    required this.diskUsedGb,
    required this.diskTotalGb,
    required this.netRxKbps,
    required this.netTxKbps,
    required this.uptimeSecs,
  });

  final double cpuUsage;
  final int cpuCores;
  final int ramUsedMb;
  final int ramTotalMb;
  final double diskUsedGb;
  final double diskTotalGb;
  final double netRxKbps;
  final double netTxKbps;
  final int uptimeSecs;

  int get ramAvailableMb => (ramTotalMb - ramUsedMb).clamp(0, ramTotalMb);
  double get diskAvailableGb => (diskTotalGb - diskUsedGb).clamp(0.0, diskTotalGb);
  double get ramUsedFraction => ramTotalMb == 0 ? 0 : ramUsedMb / ramTotalMb;
  double get diskUsedFraction => diskTotalGb == 0 ? 0 : diskUsedGb / diskTotalGb;

  factory Telemetry.fromJson(Map<String, dynamic>? json) {
    final j = json ?? const {};
    return Telemetry(
      cpuUsage: _asDouble(j['cpu_usage']),
      cpuCores: _asInt(j['cpu_cores']),
      ramUsedMb: _asInt(j['ram_used_mb']),
      ramTotalMb: _asInt(j['ram_total_mb']),
      diskUsedGb: _asDouble(j['disk_used_gb']),
      diskTotalGb: _asDouble(j['disk_total_gb']),
      netRxKbps: _asDouble(j['net_rx_kbps']),
      netTxKbps: _asDouble(j['net_tx_kbps']),
      uptimeSecs: _asInt(j['uptime_secs']),
    );
  }

  static const empty = Telemetry(
    cpuUsage: 0, cpuCores: 0, ramUsedMb: 0, ramTotalMb: 0,
    diskUsedGb: 0, diskTotalGb: 0, netRxKbps: 0, netTxKbps: 0, uptimeSecs: 0,
  );
}

class SharedResources {
  const SharedResources({
    required this.enabled,
    required this.cpuOfferedPct,
    required this.ramOfferedMb,
    required this.diskOfferedGb,
  });

  final bool enabled;
  final double cpuOfferedPct;
  final int ramOfferedMb;
  final double diskOfferedGb;

  factory SharedResources.fromJson(Map<String, dynamic>? json) {
    final j = json ?? const {};
    return SharedResources(
      enabled: j['enabled'] as bool? ?? false,
      cpuOfferedPct: _asDouble(j['cpu_offered_pct']),
      ramOfferedMb: _asInt(j['ram_offered_mb']),
      diskOfferedGb: _asDouble(j['disk_offered_gb']),
    );
  }

  static const empty = SharedResources(
      enabled: false, cpuOfferedPct: 0, ramOfferedMb: 0, diskOfferedGb: 0);
}

class Peer {
  const Peer({
    required this.nodeId,
    required this.username,
    required this.alias,
    required this.ip,
    required this.deviceType,
    required this.online,
    required this.telemetry,
    required this.shared,
    required this.latencyMs,
    required this.dashboardUrl,
  });

  final String nodeId;
  final String username;
  final String? alias;
  final String ip;
  final String deviceType;
  final bool online;
  final Telemetry telemetry;
  final SharedResources shared;
  final double? latencyMs;
  final String dashboardUrl;

  String get displayName => (alias?.isNotEmpty ?? false) ? alias! : username;

  factory Peer.fromJson(Map<String, dynamic> json) {
    final ports = json['ports'] as Map<String, dynamic>? ?? const {};
    final ip = json['ip'] as String? ?? '';
    return Peer(
      nodeId: json['node_id'] as String? ?? '',
      username: json['username'] as String? ?? 'unknown',
      alias: json['alias'] as String?,
      ip: ip,
      deviceType: json['device_type'] as String? ?? 'device',
      online: json['online'] as bool? ?? false,
      telemetry: Telemetry.fromJson(json['telemetry'] as Map<String, dynamic>?),
      shared: SharedResources.fromJson(json['shared'] as Map<String, dynamic>?),
      latencyMs: (json['latency_ms'] as num?)?.toDouble(),
      dashboardUrl: 'http://$ip:${_asInt(ports['web'])}',
    );
  }
}

class ChatMessage {
  const ChatMessage({
    required this.messageId,
    required this.senderId,
    required this.senderName,
    required this.recipientId,
    required this.content,
    required this.timestamp,
  });

  final String messageId;
  final String senderId;
  final String senderName;
  final String? recipientId;
  final String content;
  final DateTime timestamp;

  bool get isDirect => recipientId != null;

  factory ChatMessage.fromJson(Map<String, dynamic> json) => ChatMessage(
        messageId: json['message_id'] as String? ?? '',
        senderId: json['sender_id'] as String? ?? '',
        senderName: json['sender_name'] as String? ?? 'unknown',
        recipientId: json['recipient_id'] as String?,
        content: json['content'] as String? ?? '',
        timestamp:
            DateTime.tryParse(json['timestamp'] as String? ?? '')?.toLocal() ??
                DateTime.now(),
      );
}

/// Mirrors the Rust `TransferState`, which serialises as
/// `{"state": "streaming"}` or `{"state": "failed", "detail": "…"}`.
class TransferState {
  const TransferState(this.name, this.detail);
  final String name;
  final String? detail;

  bool get isDone => name == 'completed';
  bool get isFailed => name == 'failed' || name == 'rejected';
  bool get isActive => !isDone && !isFailed;

  String get label => const {
        'offered': 'Offered',
        'accepted': 'Accepted',
        'streaming': 'Transferring',
        'verifying': 'Verifying',
        'completed': 'Done',
        'rejected': 'Declined',
        'failed': 'Failed',
      }[name] ??
      name;

  factory TransferState.fromJson(Map<String, dynamic>? json) => TransferState(
        json?['state'] as String? ?? 'unknown',
        json?['detail'] as String?,
      );
}

class FileTransfer {
  const FileTransfer({
    required this.transferId,
    required this.peerName,
    required this.filename,
    required this.incoming,
    required this.state,
    required this.bytesTotal,
    required this.bytesDone,
    required this.rateBps,
    required this.savedPath,
  });

  final String transferId;
  final String peerName;
  final String filename;
  final bool incoming;
  final TransferState state;
  final int bytesTotal;
  final int bytesDone;
  final double rateBps;
  final String? savedPath;

  double get progress =>
      bytesTotal == 0 ? (state.isDone ? 1 : 0) : (bytesDone / bytesTotal).clamp(0, 1);

  factory FileTransfer.fromJson(Map<String, dynamic> json) => FileTransfer(
        transferId: json['transfer_id'] as String? ?? '',
        peerName: json['peer_name'] as String? ?? 'unknown',
        filename: json['filename'] as String? ?? 'file',
        incoming: json['direction'] == 'incoming',
        state: TransferState.fromJson(json['state'] as Map<String, dynamic>?),
        bytesTotal: _asInt(json['bytes_total']),
        bytesDone: _asInt(json['bytes_done']),
        rateBps: _asDouble(json['rate_bps']),
        savedPath: json['saved_path'] as String?,
      );
}

class CallSession {
  const CallSession({
    required this.sessionId,
    required this.peerId,
    required this.peerName,
    required this.outgoing,
    required this.state,
    required this.framesSent,
    required this.framesReceived,
  });

  final String sessionId;
  final String peerId;
  final String peerName;
  final bool outgoing;
  final String state;
  final int framesSent;
  final int framesReceived;

  bool get isLive => state == 'ringing' || state == 'connected';
  bool get isConnected => state == 'connected';

  factory CallSession.fromJson(Map<String, dynamic> json) => CallSession(
        sessionId: json['session_id'] as String? ?? '',
        peerId: json['peer_id'] as String? ?? '',
        peerName: json['peer_name'] as String? ?? 'unknown',
        outgoing: json['outgoing'] as bool? ?? false,
        state: json['state'] as String? ?? 'ended',
        framesSent: _asInt(json['frames_sent']),
        framesReceived: _asInt(json['frames_received']),
      );
}

class PoolTotals {
  const PoolTotals({
    required this.nodes,
    required this.sharingNodes,
    required this.cpuCores,
    required this.ramTotalMb,
    required this.ramAvailableMb,
    required this.diskTotalGb,
    required this.diskAvailableGb,
  });

  final int nodes;
  final int sharingNodes;
  final int cpuCores;
  final int ramTotalMb;
  final int ramAvailableMb;
  final double diskTotalGb;
  final double diskAvailableGb;

  factory PoolTotals.fromJson(Map<String, dynamic>? json) {
    final j = json ?? const {};
    return PoolTotals(
      nodes: _asInt(j['nodes']),
      sharingNodes: _asInt(j['sharing_nodes']),
      cpuCores: _asInt(j['cpu_cores']),
      ramTotalMb: _asInt(j['ram_total_mb']),
      ramAvailableMb: _asInt(j['ram_available_mb']),
      diskTotalGb: _asDouble(j['disk_total_gb']),
      diskAvailableGb: _asDouble(j['disk_available_gb']),
    );
  }

  static const empty = PoolTotals(
    nodes: 0, sharingNodes: 0, cpuCores: 0, ramTotalMb: 0,
    ramAvailableMb: 0, diskTotalGb: 0, diskAvailableGb: 0,
  );
}

class NodeSnapshot {
  const NodeSnapshot({
    required this.nodeId,
    required this.username,
    required this.deviceType,
    required this.ip,
    required this.dashboardUrl,
    required this.telemetry,
    required this.shared,
    required this.sharingEnabled,
    required this.peers,
    required this.onlinePeers,
    required this.pool,
    required this.chats,
    required this.transfers,
    required this.calls,
    required this.downloadDir,
    required this.version,
  });

  final String nodeId;
  final String username;
  final String deviceType;
  final String ip;
  final String dashboardUrl;
  final Telemetry telemetry;
  final SharedResources shared;
  final bool sharingEnabled;
  final List<Peer> peers;
  final int onlinePeers;
  final PoolTotals pool;
  final List<ChatMessage> chats;
  final List<FileTransfer> transfers;
  final List<CallSession> calls;
  final String downloadDir;
  final String version;

  List<Peer> get onlinePeerList => peers.where((p) => p.online).toList();

  factory NodeSnapshot.fromJson(Map<String, dynamic> json) {
    final identity = json['identity'] as Map<String, dynamic>? ?? const {};
    List<T> list<T>(String key, T Function(Map<String, dynamic>) parse) =>
        ((json[key] as List<dynamic>?) ?? const [])
            .map((e) => parse(e as Map<String, dynamic>))
            .toList();

    return NodeSnapshot(
      nodeId: identity['node_id'] as String? ?? '',
      username: identity['username'] as String? ?? 'this device',
      deviceType: identity['device_type'] as String? ?? 'device',
      ip: json['ip'] as String? ?? '',
      dashboardUrl: json['dashboard_url'] as String? ?? '',
      telemetry: Telemetry.fromJson(json['telemetry'] as Map<String, dynamic>?),
      shared: SharedResources.fromJson(json['shared'] as Map<String, dynamic>?),
      sharingEnabled: json['sharing_enabled'] as bool? ?? false,
      peers: list('peers', Peer.fromJson),
      onlinePeers: _asInt(json['online_peers']),
      pool: PoolTotals.fromJson(json['pool'] as Map<String, dynamic>?),
      chats: list('chats', ChatMessage.fromJson),
      transfers: list('transfers', FileTransfer.fromJson),
      calls: list('calls', CallSession.fromJson),
      downloadDir: json['download_dir'] as String? ?? '',
      version: json['version'] as String? ?? '',
    );
  }

  static NodeSnapshot parse(String raw) =>
      NodeSnapshot.fromJson(jsonDecode(raw) as Map<String, dynamic>);
}

/// One event from the node's bus.
///
/// The Rust enum serialises as `{"event": "chat", "data": {…}}`.
class NodeEvent {
  const NodeEvent(this.kind, this.data);
  final String kind;
  final Map<String, dynamic> data;

  bool get isChat => kind == 'chat';
  bool get isNotice => kind == 'notice';
  bool get isTransfer => kind == 'transfer';

  /// Severity for a notice: `info`, `success`, `warning` or `error`.
  String get level => data['level'] as String? ?? 'info';
  String get text => data['text'] as String? ?? '';

  ChatMessage? get chat => isChat ? ChatMessage.fromJson(data) : null;

  factory NodeEvent.fromJson(Map<String, dynamic> json) => NodeEvent(
        json['event'] as String? ?? 'unknown',
        (json['data'] as Map<String, dynamic>?) ?? const {},
      );
}
