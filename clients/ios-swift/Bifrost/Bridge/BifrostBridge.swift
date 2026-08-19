// Swift wrapper over the Bifrost C ABI.
//
// The C functions are reachable directly through the bridging header; this
// file exists so that the ownership rules are obeyed in exactly one place.
// Every string the library returns is freed here, and every string passed in
// is converted safely.

import Foundation

enum BifrostError: LocalizedError {
    case native(String)

    var errorDescription: String? {
        switch self {
        case .native(let message): return message
        }
    }
}

/// Low-level access to libbifrost_core.
enum BifrostBridge {

    // MARK: - String handling

    /// Read a returned C string and free the native allocation.
    ///
    /// The `defer` runs even if `String(cString:)` throws, so a malformed
    /// reply cannot leak.
    private static func takeString(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
        guard let pointer else { return nil }
        defer { bifrost_string_free(pointer) }
        return String(cString: pointer)
    }

    /// The library's most recent error message.
    static func lastError() -> String {
        let message = takeString(bifrost_last_error()) ?? ""
        return message.isEmpty ? "unknown error" : message
    }

    /// Turn a -1 status into a thrown error.
    private static func check(_ status: Int32, _ what: String) throws {
        if status < 0 {
            throw BifrostError.native("\(what) failed: \(lastError())")
        }
    }

    // MARK: - Lifecycle

    /// Start the node, storing its config in the app's Documents directory.
    ///
    /// iOS gives an app no writable working directory, so the path must be
    /// explicit — passing NULL would fail on device.
    static func start() throws {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let directory = documents.appendingPathComponent("bifrost", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

        let configPath = directory.appendingPathComponent("config.json").path
        let status = configPath.withCString { bifrost_start($0) }

        // 1 means "already running", which is success here: SwiftUI previews
        // and scene reconnection both re-enter this path.
        if status < 0 {
            throw BifrostError.native("could not start Bifrost: \(lastError())")
        }
    }

    static func stop() {
        _ = bifrost_stop()
    }

    static var isRunning: Bool { bifrost_is_running() != 0 }

    static var version: String { takeString(bifrost_version()) ?? "unknown" }

    static var protocolVersion: Int32 { bifrost_protocol_version() }

    // MARK: - Reading state

    static func snapshot() -> NodeSnapshot? {
        guard let json = takeString(bifrost_snapshot_json()),
              let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder.bifrost.decode(NodeSnapshot.self, from: data)
    }

    static func peers() -> [Peer] {
        decodeArray(takeString(bifrost_peers_json()))
    }

    static func chat(limit: Int32 = 200) -> [ChatMessage] {
        decodeArray(takeString(bifrost_chat_json(limit)))
    }

    static func transfers() -> [FileTransfer] {
        decodeArray(takeString(bifrost_transfers_json()))
    }

    static func calls() -> [CallSession] {
        decodeArray(takeString(bifrost_calls_json()))
    }

    static func pollEvents() -> [NodeEvent] {
        decodeArray(takeString(bifrost_poll_events()))
    }

    private static func decodeArray<T: Decodable>(_ json: String?) -> [T] {
        guard let json, let data = json.data(using: .utf8) else { return [] }
        return (try? JSONDecoder.bifrost.decode([T].self, from: data)) ?? []
    }

    // MARK: - Actions

    static func broadcast(_ message: String) throws {
        let status = message.withCString { bifrost_send_chat($0, nil) }
        try check(status, "sending the message")
    }

    static func directMessage(_ message: String, to peerID: String) throws {
        let status = message.withCString { content in
            peerID.withCString { recipient in
                bifrost_send_chat(content, recipient)
            }
        }
        try check(status, "sending the message")
    }

    static func sendFile(_ path: String, to peerID: String) throws {
        let status = peerID.withCString { peer in
            path.withCString { file in
                bifrost_send_file(peer, file)
            }
        }
        try check(status, "sending the file")
    }

    static func setAlias(_ alias: String?, for peerID: String) throws {
        let status = peerID.withCString { peer -> Int32 in
            guard let alias else { return bifrost_set_alias(peer, nil) }
            return alias.withCString { bifrost_set_alias(peer, $0) }
        }
        try check(status, "setting the alias")
    }

    static func setSharing(_ enabled: Bool) throws {
        try check(bifrost_set_sharing(enabled ? 1 : 0), "toggling sharing")
    }

    static func rename(_ name: String) throws {
        try check(name.withCString { bifrost_rename($0) }, "renaming this device")
    }

    /// Run a command line and return its text reply.
    @discardableResult
    static func execute(_ line: String) -> String {
        guard let raw = takeString(line.withCString { bifrost_execute($0) }),
              raw != "null",
              let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return "" }

        return object["text"] as? String ?? ""
    }

    // MARK: - Voice

    static func startCall(with peerID: String) throws -> String {
        guard let session = takeString(peerID.withCString { bifrost_call($0) }) else {
            throw BifrostError.native("could not call: \(lastError())")
        }
        return session
    }

    static func hangUp(session: String? = nil) throws {
        let status: Int32
        if let session {
            status = session.withCString { bifrost_hangup($0) }
        } else {
            status = bifrost_hangup(nil)
        }
        try check(status, "ending the call")
    }

    /// Send one 20 ms frame of signed 16-bit little-endian mono PCM at 16 kHz.
    static func sendAudio(_ pcm: Data, session: String) throws {
        let status = session.withCString { sessionPointer in
            pcm.withUnsafeBytes { buffer -> Int32 in
                guard let base = buffer.bindMemory(to: UInt8.self).baseAddress else {
                    return -1
                }
                return bifrost_voice_send_frame(sessionPointer, base, buffer.count)
            }
        }
        try check(status, "sending audio")
    }

    /// Drain received audio for a call.
    static func takeAudio(session: String) -> [Data] {
        guard let json = takeString(session.withCString { bifrost_voice_take_frames($0) }),
              let data = json.data(using: .utf8),
              let encoded = try? JSONDecoder().decode([String].self, from: data)
        else { return [] }

        return encoded.compactMap { Data(base64Encoded: $0) }
    }
}

extension JSONDecoder {
    /// Decoder matching the core's serialisation: snake_case keys and RFC 3339
    /// timestamps.
    static let bifrost: JSONDecoder = {
        let decoder = JSONDecoder()
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]

        decoder.dateDecodingStrategy = .custom { decoder in
            let text = try decoder.singleValueContainer().decode(String.self)
            if let date = formatter.date(from: text) { return date }

            // Chrono omits fractional seconds when they are zero.
            let plain = ISO8601DateFormatter()
            plain.formatOptions = [.withInternetDateTime]
            if let date = plain.date(from: text) { return date }

            throw DecodingError.dataCorruptedError(
                in: try decoder.singleValueContainer(),
                debugDescription: "unrecognised timestamp: \(text)"
            )
        }
        return decoder
    }()
}
