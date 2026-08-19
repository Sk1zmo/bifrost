// The observable store every view reads from.

import Combine
import Foundation
import SwiftUI

/// One line in the activity log.
struct LogLine: Identifiable {
    let id = UUID()
    let text: String
    let level: String
    let at = Date()

    var colour: Color {
        switch level {
        case "success": return Theme.good
        case "error": return Theme.critical
        case "warning": return Theme.warning
        default: return .primary
        }
    }
}

@MainActor
final class NodeStore: ObservableObject {
    @Published private(set) var snapshot: NodeSnapshot?
    @Published private(set) var log: [LogLine] = []
    @Published private(set) var startupError: String?
    @Published private(set) var isStarting = true

    private var timer: Timer?

    var peers: [Peer] { snapshot?.peers ?? [] }
    var onlinePeers: [Peer] { snapshot?.onlinePeerList ?? [] }
    var chats: [ChatMessage] { snapshot?.chats ?? [] }
    var transfers: [FileTransfer] { snapshot?.transfers ?? [] }
    var calls: [CallSession] { snapshot?.calls ?? [] }
    var telemetry: Telemetry { snapshot?.telemetry ?? Telemetry() }
    var pool: PoolTotals { snapshot?.pool ?? PoolTotals() }
    var activeCall: CallSession? { calls.first(where: \.isConnected) }

    /// Start the node and begin polling.
    func start() async {
        isStarting = true
        startupError = nil

        do {
            // Starting blocks briefly while sockets bind; keep it off the
            // main actor so the launch screen keeps animating.
            try await Task.detached(priority: .userInitiated) {
                try BifrostBridge.start()
            }.value

            refresh()
            push("Bifrost started", level: "success")
            beginPolling()
        } catch {
            startupError = error.localizedDescription
        }

        isStarting = false
    }

    func stop() {
        timer?.invalidate()
        timer = nil
        BifrostBridge.stop()
    }

    /// Poll for events and a snapshot on one timer.
    ///
    /// The core buffers events rather than calling back into Swift: a callback
    /// from a Rust thread would need its own run-loop plumbing, and polling
    /// from a timer the UI already has cannot deadlock.
    private func beginPolling() {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 0.9, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tick() }
        }
    }

    private func tick() {
        for event in BifrostBridge.pollEvents() {
            if event.isNotice {
                push(event.data?.text ?? "", level: event.data?.level ?? "info")
            } else if event.isChat, event.data?.senderId != snapshot?.identity.nodeId {
                let name = event.data?.senderName ?? "someone"
                push("\(name): \(event.data?.content ?? "")", level: "info")
            }
        }
        refresh()
    }

    func refresh() {
        if let latest = BifrostBridge.snapshot() {
            snapshot = latest
        }
    }

    private func push(_ text: String, level: String) {
        guard !text.isEmpty else { return }
        log.insert(LogLine(text: text, level: level), at: 0)
        if log.count > 200 { log.removeLast(log.count - 200) }
    }

    /// Run an action, reporting failure in the log rather than throwing into
    /// a SwiftUI action closure.
    private func guarded(_ what: String, _ action: () throws -> Void) {
        do {
            try action()
            refresh()
        } catch {
            push("\(what) failed: \(error.localizedDescription)", level: "error")
        }
    }

    func broadcast(_ message: String) {
        let trimmed = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }

        // A slash command is a command wherever it is typed.
        if trimmed.hasPrefix("/") {
            let reply = BifrostBridge.execute(trimmed)
            if !reply.isEmpty { push(reply, level: "info") }
            refresh()
        } else {
            guarded("Sending") { try BifrostBridge.broadcast(trimmed) }
        }
    }

    func directMessage(_ message: String, to peerID: String) {
        let trimmed = message.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        guarded("Sending") { try BifrostBridge.directMessage(trimmed, to: peerID) }
    }

    func sendFile(_ path: String, to peerID: String) {
        guarded("Sending the file") { try BifrostBridge.sendFile(path, to: peerID) }
    }

    func setAlias(_ alias: String?, for peerID: String) {
        guarded("Renaming") { try BifrostBridge.setAlias(alias, for: peerID) }
    }

    func setSharing(_ enabled: Bool) {
        guarded("Toggling sharing") { try BifrostBridge.setSharing(enabled) }
    }

    func rename(_ name: String) {
        guarded("Renaming this device") { try BifrostBridge.rename(name) }
    }

    func startCall(with peerID: String) {
        guarded("Calling") {
            let session = try BifrostBridge.startCall(with: peerID)
            push("In a call (session \(session.prefix(8)))", level: "success")
        }
    }

    func hangUp(session: String? = nil) {
        guarded("Ending the call") { try BifrostBridge.hangUp(session: session) }
    }

    func ping(_ peerID: String) {
        let reply = BifrostBridge.execute("/ping \(peerID)")
        if !reply.isEmpty { push(reply, level: "info") }
    }
}

/// Palette shared with the web dashboard, so the two read as one product.
enum Theme {
    static let seriesOne = Color(red: 0.165, green: 0.471, blue: 0.839)  // #2A78D6
    static let seriesTwo = Color(red: 0.922, green: 0.408, blue: 0.204)  // #EB6834
    static let good = Color(red: 0.047, green: 0.639, blue: 0.047)       // #0CA30C
    static let warning = Color(red: 0.980, green: 0.698, blue: 0.098)    // #FAB219
    static let critical = Color(red: 0.816, green: 0.231, blue: 0.231)   // #D03B3B

    /// Severity for a fraction. Always shown beside the printed value, so
    /// colour is never the only cue.
    static func severity(_ fraction: Double) -> Color {
        fraction >= 0.9 ? critical : fraction >= 0.75 ? warning : seriesOne
    }
}

/// Formatting helpers shared across the views.
enum Format {
    static func bytes(_ value: Double) -> String {
        guard value > 0 else { return "0 B" }
        let units = ["B", "KB", "MB", "GB", "TB"]
        var value = value
        var unit = 0
        while value >= 1024, unit < units.count - 1 {
            value /= 1024
            unit += 1
        }
        return value >= 100 || unit == 0
            ? "\(Int(value.rounded())) \(units[unit])"
            : String(format: "%.1f %@", value, units[unit])
    }

    static func gigabytes(fromMegabytes mb: Int) -> String {
        String(format: "%.1f GB", Double(mb) / 1024)
    }

    static func deviceIcon(_ deviceType: String) -> String {
        let d = deviceType.lowercased()
        if d.contains("phone") || d.contains("android") || d.contains("ios") { return "iphone" }
        if d.contains("tablet") || d.contains("ipad") { return "ipad" }
        if d.contains("server") || d.contains("pi") { return "server.rack" }
        if d.contains("mac") { return "laptopcomputer" }
        if d.contains("windows") { return "desktopcomputer" }
        if d.contains("linux") { return "laptopcomputer" }
        return "display"
    }
}
