// The SwiftUI interface.

import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var store: NodeStore

    var body: some View {
        Group {
            if store.isStarting {
                VStack(spacing: 14) {
                    ProgressView()
                    Text("Joining the network…").foregroundStyle(.secondary)
                }
            } else if let error = store.startupError {
                StartupFailure(message: error)
            } else {
                TabView {
                    DevicesTab()
                        .tabItem { Label("Devices", systemImage: "rectangle.stack") }
                        .badge(store.onlinePeers.count)
                    ChatTab()
                        .tabItem { Label("Chat", systemImage: "bubble.left.and.bubble.right") }
                    TransfersTab()
                        .tabItem { Label("Files", systemImage: "folder") }
                    SettingsTab()
                        .tabItem { Label("Settings", systemImage: "gearshape") }
                }
            }
        }
        .task { await store.start() }
    }
}

private struct StartupFailure: View {
    let message: String
    @EnvironmentObject private var store: NodeStore

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "wifi.slash").font(.system(size: 44))
            Text("Bifrost could not start").font(.headline)
            Text(message)
                .font(.footnote)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Button("Try again") { Task { await store.start() } }
                .buttonStyle(.borderedProminent)
        }
        .padding(28)
    }
}

// MARK: - Devices

struct DevicesTab: View {
    @EnvironmentObject private var store: NodeStore

    var body: some View {
        NavigationStack {
            List {
                Section {
                    HeroRow(pool: store.pool)
                    if let snapshot = store.snapshot {
                        SelfRow(snapshot: snapshot)
                    }
                }

                Section("Devices on this network") {
                    if store.peers.isEmpty {
                        Text("No other devices yet. Start Bifrost on a laptop on this "
                             + "same Wi-Fi — it will appear within a few seconds.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(store.peers) { peer in PeerRow(peer: peer) }
                    }
                }
            }
            .navigationTitle("Bifrost")
            .refreshable { store.refresh() }
        }
    }
}

private struct HeroRow: View {
    let pool: PoolTotals

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .lastTextBaseline, spacing: 8) {
                Text("\(pool.nodes)").font(.system(size: 44, weight: .semibold))
                Text(pool.nodes == 1 ? "device (just this one)" : "devices on this network")
                    .foregroundStyle(.secondary)
            }
            HStack(spacing: 20) {
                Stat(label: "Cores", value: "\(pool.cpuCores)")
                Stat(label: "RAM free", value: Format.gigabytes(fromMegabytes: pool.ramAvailableMb))
                Stat(label: "Disk free", value: "\(Int(pool.diskAvailableGb.rounded())) GB")
                Stat(label: "Sharing", value: "\(pool.sharingNodes)")
            }
        }
        .padding(.vertical, 6)
    }
}

private struct Stat: View {
    let label: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label).font(.caption2).foregroundStyle(.secondary)
            Text(value).font(.callout.weight(.semibold))
        }
    }
}

private struct SelfRow: View {
    let snapshot: NodeSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Image(systemName: Format.deviceIcon(snapshot.identity.deviceType))
                Text(snapshot.identity.username).fontWeight(.semibold)
                Spacer()
                Text(snapshot.ip).font(.caption).foregroundStyle(.secondary)
            }
            Meter(label: "CPU",
                  fraction: snapshot.telemetry.cpuUsage / 100,
                  value: "\(Int(snapshot.telemetry.cpuUsage))% · \(snapshot.telemetry.cpuCores) cores")
            Meter(label: "RAM",
                  fraction: snapshot.telemetry.ramUsedFraction,
                  value: "\(Format.gigabytes(fromMegabytes: snapshot.telemetry.ramUsedMb)) / "
                       + Format.gigabytes(fromMegabytes: snapshot.telemetry.ramTotalMb))
            Meter(label: "Disk",
                  fraction: snapshot.telemetry.diskUsedFraction,
                  value: "\(Int(snapshot.telemetry.diskUsedGb)) / \(Int(snapshot.telemetry.diskTotalGb)) GB")
        }
        .padding(.vertical, 4)
    }
}

private struct PeerRow: View {
    let peer: Peer
    @EnvironmentObject private var store: NodeStore
    @State private var renaming = false
    @State private var draftAlias = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Image(systemName: Format.deviceIcon(peer.deviceType))
                VStack(alignment: .leading, spacing: 1) {
                    Text(peer.displayName).fontWeight(.semibold)
                    Text("\(peer.ip) · \(peer.deviceType)")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                // A dot plus the word, never colour alone.
                HStack(spacing: 5) {
                    Circle()
                        .fill(peer.online ? Theme.good : Color.secondary)
                        .frame(width: 8, height: 8)
                    Text(peer.online ? "Online" : "Offline")
                        .font(.caption2).foregroundStyle(.secondary)
                }
            }

            Meter(label: "CPU",
                  fraction: peer.telemetry.cpuUsage / 100,
                  value: "\(Int(peer.telemetry.cpuUsage))%")
            Meter(label: "RAM",
                  fraction: peer.telemetry.ramUsedFraction,
                  value: "\(Format.gigabytes(fromMegabytes: peer.telemetry.ramAvailableMb)) free")

            HStack(spacing: 14) {
                Button("Ping") { store.ping(peer.nodeId) }
                Button("Call") { store.startCall(with: peer.nodeId) }
                Button("Rename") {
                    draftAlias = peer.alias ?? ""
                    renaming = true
                }
                Spacer()
            }
            .font(.footnote)
            .buttonStyle(.borderless)
            .disabled(!peer.online)
        }
        .padding(.vertical, 4)
        .opacity(peer.online ? 1 : 0.55)
        .alert("Name for \(peer.username)", isPresented: $renaming) {
            TextField("Leave empty to clear", text: $draftAlias)
            Button("Cancel", role: .cancel) {}
            Button("Save") {
                let trimmed = draftAlias.trimmingCharacters(in: .whitespaces)
                store.setAlias(trimmed.isEmpty ? nil : trimmed, for: peer.nodeId)
            }
        }
    }
}

/// A labelled bar with its value printed alongside.
struct Meter: View {
    let label: String
    let fraction: Double
    let value: String

    var body: some View {
        HStack(spacing: 10) {
            Text(label).font(.caption2).foregroundStyle(.secondary).frame(width: 34, alignment: .leading)
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(Theme.severity(fraction).opacity(0.18))
                    Capsule()
                        .fill(Theme.severity(fraction))
                        .frame(width: geometry.size.width * min(max(fraction, 0), 1))
                }
            }
            .frame(height: 7)
            Text(value).font(.caption2).foregroundStyle(.secondary)
                .frame(width: 118, alignment: .trailing)
        }
    }
}
