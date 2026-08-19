// Swift mirrors of the JSON the Rust core produces.
//
// `CodingKeys` spell out the snake_case names rather than relying on a
// key-decoding strategy, so a rename on the Rust side surfaces as a compile
// error here instead of a silently-nil field.

import Foundation

struct Telemetry: Codable, Hashable {
    var cpuUsage: Double = 0
    var cpuCores: Int = 0
    var ramUsedMb: Int = 0
    var ramTotalMb: Int = 0
    var diskUsedGb: Double = 0
    var diskTotalGb: Double = 0
    var netRxKbps: Double = 0
    var netTxKbps: Double = 0
    var uptimeSecs: Int = 0

    enum CodingKeys: String, CodingKey {
        case cpuUsage = "cpu_usage"
        case cpuCores = "cpu_cores"
        case ramUsedMb = "ram_used_mb"
        case ramTotalMb = "ram_total_mb"
        case diskUsedGb = "disk_used_gb"
        case diskTotalGb = "disk_total_gb"
        case netRxKbps = "net_rx_kbps"
        case netTxKbps = "net_tx_kbps"
        case uptimeSecs = "uptime_secs"
    }

    var ramAvailableMb: Int { max(0, ramTotalMb - ramUsedMb) }
    var diskAvailableGb: Double { max(0, diskTotalGb - diskUsedGb) }
    var ramUsedFraction: Double { ramTotalMb == 0 ? 0 : Double(ramUsedMb) / Double(ramTotalMb) }
    var diskUsedFraction: Double { diskTotalGb == 0 ? 0 : diskUsedGb / diskTotalGb }
}

struct SharedResources: Codable, Hashable {
    var enabled: Bool = false
    var cpuOfferedPct: Double = 0
    var ramOfferedMb: Int = 0
    var diskOfferedGb: Double = 0

    enum CodingKeys: String, CodingKey {
        case enabled
        case cpuOfferedPct = "cpu_offered_pct"
        case ramOfferedMb = "ram_offered_mb"
        case diskOfferedGb = "disk_offered_gb"
    }
}

struct PortSet: Codable, Hashable {
    var web: Int = 9000
    var discovery: Int = 9001
    var messaging: Int = 9002
    var files: Int = 9003
    var voiceSignal: Int = 9004
    var voiceAudio: Int = 9005

    enum CodingKeys: String, CodingKey {
        case web, discovery, messaging, files
        case voiceSignal = "voice_signal"
        case voiceAudio = "voice_audio"
    }
}

struct Identity: Codable, Hashable {
    var nodeId: String = ""
    var username: String = ""
    var deviceType: String = ""
    var os: String = ""

    enum CodingKeys: String, CodingKey {
        case nodeId = "node_id"
        case username
        case deviceType = "device_type"
        case os
    }
}

struct Peer: Codable, Identifiable, Hashable {
    var nodeId: String
    var username: String
    var alias: String?
    var ip: String
    var deviceType: String
    var online: Bool
    var telemetry: Telemetry
    var shared: SharedResources
    var latencyMs: Double?
    var ports: PortSet

    var id: String { nodeId }
    var displayName: String { alias?.isEmpty == false ? alias! : username }
    var dashboardURL: URL? { URL(string: "http://\(ip):\(ports.web)") }

    enum CodingKeys: String, CodingKey {
        case nodeId = "node_id"
        case username, alias, ip, online, telemetry, shared, ports
        case deviceType = "device_type"
        case latencyMs = "latency_ms"
    }
}

struct ChatMessage: Codable, Identifiable, Hashable {
    var messageId: String
    var senderId: String
    var senderName: String
    var recipientId: String?
    var content: String
    var timestamp: Date

    var id: String { messageId }
    var isDirect: Bool { recipientId != nil }

    enum CodingKeys: String, CodingKey {
        case messageId = "message_id"
        case senderId = "sender_id"
        case senderName = "sender_name"
        case recipientId = "recipient_id"
        case content, timestamp
    }
}

/// Mirrors the Rust `TransferState`, serialised as
/// `{"state": "streaming"}` or `{"state": "failed", "detail": "…"}`.
struct TransferState: Codable, Hashable {
    var state: String
    var detail: String?

    var isDone: Bool { state == "completed" }
    var isFailed: Bool { state == "failed" || state == "rejected" }
    var isActive: Bool { !isDone && !isFailed }

    var label: String {
        [
            "offered": "Offered", "accepted": "Accepted", "streaming": "Transferring",
            "verifying": "Verifying", "completed": "Done", "rejected": "Declined",
            "failed": "Failed",
        ][state] ?? state
    }
}

struct FileTransfer: Codable, Identifiable, Hashable {
    var transferId: String
    var peerName: String
    var filename: String
    var direction: String
    var state: TransferState
    var bytesTotal: Int
    var bytesDone: Int
    var rateBps: Double
    var savedPath: String?

    var id: String { transferId }
    var isIncoming: Bool { direction == "incoming" }
    var progress: Double {
        bytesTotal == 0 ? (state.isDone ? 1 : 0)
                        : min(1, Double(bytesDone) / Double(bytesTotal))
    }

    enum CodingKeys: String, CodingKey {
        case transferId = "transfer_id"
        case peerName = "peer_name"
        case filename, direction, state
        case bytesTotal = "bytes_total"
        case bytesDone = "bytes_done"
        case rateBps = "rate_bps"
        case savedPath = "saved_path"
    }
}

struct CallSession: Codable, Identifiable, Hashable {
    var sessionId: String
    var peerId: String
    var peerName: String
    var outgoing: Bool
    var state: String
    var framesSent: Int
    var framesReceived: Int

    var id: String { sessionId }
    var isLive: Bool { state == "ringing" || state == "connected" }
    var isConnected: Bool { state == "connected" }

    enum CodingKeys: String, CodingKey {
        case sessionId = "session_id"
        case peerId = "peer_id"
        case peerName = "peer_name"
        case outgoing, state
        case framesSent = "frames_sent"
        case framesReceived = "frames_received"
    }
}

struct PoolTotals: Codable, Hashable {
    var nodes: Int = 0
    var sharingNodes: Int = 0
    var cpuCores: Int = 0
    var ramTotalMb: Int = 0
    var ramAvailableMb: Int = 0
    var diskTotalGb: Double = 0
    var diskAvailableGb: Double = 0

    enum CodingKeys: String, CodingKey {
        case nodes
        case sharingNodes = "sharing_nodes"
        case cpuCores = "cpu_cores"
        case ramTotalMb = "ram_total_mb"
        case ramAvailableMb = "ram_available_mb"
        case diskTotalGb = "disk_total_gb"
        case diskAvailableGb = "disk_available_gb"
    }
}

struct HostSummary: Codable, Hashable {
    var hostname: String = ""
    var os: String = ""
    var arch: String = ""
    var kernel: String = ""
}

struct NodeSnapshot: Codable, Hashable {
    var identity: Identity
    var host: HostSummary
    var ports: PortSet
    var ip: String
    var dashboardUrl: String
    var telemetry: Telemetry
    var shared: SharedResources
    var sharingEnabled: Bool
    var peers: [Peer]
    var onlinePeers: Int
    var pool: PoolTotals
    var chats: [ChatMessage]
    var transfers: [FileTransfer]
    var calls: [CallSession]
    var downloadDir: String
    var uptimeSecs: Int
    var version: String

    var onlinePeerList: [Peer] { peers.filter(\.online) }

    enum CodingKeys: String, CodingKey {
        case identity, host, ports, ip, telemetry, shared, peers, pool, chats
        case transfers, calls, version
        case dashboardUrl = "dashboard_url"
        case sharingEnabled = "sharing_enabled"
        case onlinePeers = "online_peers"
        case downloadDir = "download_dir"
        case uptimeSecs = "uptime_secs"
    }
}

/// One event from the node's bus, serialised as
/// `{"event": "chat", "data": {…}}`.
struct NodeEvent: Codable {
    var event: String
    var data: EventData?

    var isChat: Bool { event == "chat" }
    var isNotice: Bool { event == "notice" }

    /// Only the fields any consumer here reads; the rest are ignored.
    struct EventData: Codable {
        var level: String?
        var text: String?
        var senderName: String?
        var senderId: String?
        var content: String?

        enum CodingKeys: String, CodingKey {
            case level, text, content
            case senderName = "sender_name"
            case senderId = "sender_id"
        }
    }
}
