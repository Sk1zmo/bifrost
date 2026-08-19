// The chat, files and settings tabs.

import SwiftUI
import UniformTypeIdentifiers

// MARK: - Chat

struct ChatTab: View {
    @EnvironmentObject private var store: NodeStore
    @State private var draft = ""
    @State private var recipient: String?

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                Picker("To", selection: $recipient) {
                    Text("Everyone").tag(String?.none)
                    ForEach(store.onlinePeers) { peer in
                        Text(peer.displayName).tag(String?.some(peer.nodeId))
                    }
                }
                .pickerStyle(.menu)
                .padding(.horizontal)

                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 8) {
                            ForEach(store.chats) { message in
                                Bubble(
                                    message: message,
                                    mine: message.senderId == store.snapshot?.identity.nodeId
                                )
                                .id(message.id)
                            }
                        }
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                    }
                    .onChange(of: store.chats.count) {
                        if let last = store.chats.last {
                            withAnimation { proxy.scrollTo(last.id, anchor: .bottom) }
                        }
                    }
                }

                HStack(spacing: 8) {
                    TextField(
                        recipient == nil ? "Message everyone, or /help" : "Private message",
                        text: $draft
                    )
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(send)

                    Button(action: send) {
                        Image(systemName: "arrow.up.circle.fill").font(.title2)
                    }
                    .disabled(draft.trimmingCharacters(in: .whitespaces).isEmpty)
                }
                .padding(10)
            }
            .navigationTitle("Chat")
            .navigationBarTitleDisplayMode(.inline)
        }
    }

    private func send() {
        let text = draft.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty else { return }

        // A slash command runs as a command even when a recipient is chosen.
        if let recipient, !text.hasPrefix("/") {
            store.directMessage(text, to: recipient)
        } else {
            store.broadcast(text)
        }
        draft = ""
    }
}

private struct Bubble: View {
    let message: ChatMessage
    let mine: Bool

    var body: some View {
        HStack {
            if mine { Spacer(minLength: 40) }

            VStack(alignment: mine ? .trailing : .leading, spacing: 2) {
                Text("\(mine ? "You" : message.senderName)"
                     + (message.isDirect ? " · private" : "")
                     + " · " + message.timestamp.formatted(date: .omitted, time: .shortened))
                    .font(.caption2)
                    .foregroundStyle(.secondary)

                Text(message.content)
                    .padding(.horizontal, 13)
                    .padding(.vertical, 8)
                    .background(mine ? Theme.seriesOne : Color(.secondarySystemBackground))
                    .foregroundStyle(mine ? .white : .primary)
                    .clipShape(RoundedRectangle(cornerRadius: 15))
                    .overlay(
                        RoundedRectangle(cornerRadius: 15)
                            .strokeBorder(
                                message.isDirect ? Color.secondary.opacity(0.5) : .clear,
                                style: StrokeStyle(lineWidth: 1, dash: [4])
                            )
                    )
            }

            if !mine { Spacer(minLength: 40) }
        }
    }
}

// MARK: - Files

struct TransfersTab: View {
    @EnvironmentObject private var store: NodeStore
    @State private var target: String?
    @State private var choosingFile = false

    var body: some View {
        NavigationStack {
            List {
                Section("Send a file") {
                    Picker("To", selection: $target) {
                        Text("Choose a device").tag(String?.none)
                        ForEach(store.onlinePeers) { peer in
                            Text(peer.displayName).tag(String?.some(peer.nodeId))
                        }
                    }
                    Button {
                        choosingFile = true
                    } label: {
                        Label("Choose a file", systemImage: "square.and.arrow.up")
                    }
                    .disabled(target == nil)
                }

                Section("Transfers") {
                    if store.transfers.isEmpty {
                        Text("No transfers yet.")
                            .font(.footnote).foregroundStyle(.secondary)
                    } else {
                        ForEach(store.transfers.prefix(25)) { transfer in
                            TransferRow(transfer: transfer)
                        }
                    }
                }
            }
            .navigationTitle("Files")
            .fileImporter(
                isPresented: $choosingFile,
                allowedContentTypes: [.item],
                allowsMultipleSelection: true
            ) { result in
                guard let target, case .success(let urls) = result else { return }

                for url in urls {
                    // A file from the document picker lives outside the app's
                    // sandbox; access must be claimed and released explicitly.
                    guard url.startAccessingSecurityScopedResource() else { continue }
                    defer { url.stopAccessingSecurityScopedResource() }

                    // Copy into the sandbox first: the transfer runs in the
                    // background and would outlive the scoped access.
                    let staged = FileManager.default.temporaryDirectory
                        .appendingPathComponent(url.lastPathComponent)
                    try? FileManager.default.removeItem(at: staged)

                    do {
                        try FileManager.default.copyItem(at: url, to: staged)
                        store.sendFile(staged.path, to: target)
                    } catch {
                        continue
                    }
                }
            }
        }
    }
}

private struct TransferRow: View {
    let transfer: FileTransfer

    private var colour: Color {
        transfer.state.isFailed ? Theme.critical
            : transfer.state.isDone ? Theme.good
            : Theme.seriesOne
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Image(systemName: transfer.isIncoming ? "arrow.down" : "arrow.up")
                    .foregroundStyle(colour)
                Text(transfer.filename).lineLimit(1).fontWeight(.medium)
                Spacer()
                // The label carries the state; the colour only reinforces it.
                Text(transfer.state.label).font(.caption).foregroundStyle(.secondary)
            }

            Text("\(transfer.peerName) · \(Format.bytes(Double(transfer.bytesDone))) of "
                 + Format.bytes(Double(transfer.bytesTotal)))
                .font(.caption2).foregroundStyle(.secondary)

            ProgressView(value: transfer.progress).tint(colour)
        }
        .padding(.vertical, 3)
    }
}

// MARK: - Settings

struct SettingsTab: View {
    @EnvironmentObject private var store: NodeStore
    @State private var name = ""
    @State private var nameLoaded = false

    var body: some View {
        NavigationStack {
            List {
                let liveCalls = store.calls.filter(\.isLive)
                if !liveCalls.isEmpty {
                    Section("Active calls") {
                        ForEach(liveCalls) { call in
                            HStack {
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(call.peerName).fontWeight(.medium)
                                    Text("\(call.state) · \(call.framesSent) sent / "
                                         + "\(call.framesReceived) received")
                                        .font(.caption2).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Button("End") { store.hangUp(session: call.sessionId) }
                                    .buttonStyle(.borderless)
                            }
                        }
                    }
                }

                Section("This device") {
                    HStack {
                        TextField("Display name", text: $name)
                        Button("Save") {
                            let trimmed = name.trimmingCharacters(in: .whitespaces)
                            if !trimmed.isEmpty { store.rename(trimmed) }
                        }
                        .buttonStyle(.borderless)
                    }

                    Toggle("Share resources", isOn: Binding(
                        get: { store.snapshot?.sharingEnabled ?? false },
                        set: { store.setSharing($0) }
                    ))

                    if let shared = store.snapshot?.shared, shared.enabled {
                        Text("Offering CPU \(Int(shared.cpuOfferedPct))% · RAM "
                             + Format.gigabytes(fromMegabytes: shared.ramOfferedMb))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }

                if let snapshot = store.snapshot {
                    Section("Connection") {
                        LabeledContent("Address", value: snapshot.ip)
                        LabeledContent("Node id", value: String(snapshot.identity.nodeId.prefix(8)))
                        LabeledContent("Version", value: "Bifrost \(snapshot.version)")
                        LabeledContent("Downloads", value: snapshot.downloadDir)
                            .font(.caption)
                    }

                    Section("Web dashboards") {
                        Text("Any device on this Wi-Fi can open a node's dashboard "
                             + "in a browser — no app needed there.")
                            .font(.caption).foregroundStyle(.secondary)
                        ForEach(store.onlinePeers) { peer in
                            if let url = peer.dashboardURL {
                                Link(peer.displayName, destination: url)
                            }
                        }
                    }
                }

                Section("Activity") {
                    if store.log.isEmpty {
                        Text("Nothing yet.").font(.footnote).foregroundStyle(.secondary)
                    } else {
                        ForEach(store.log.prefix(40)) { line in
                            HStack(alignment: .top, spacing: 8) {
                                Text(line.at.formatted(date: .omitted, time: .shortened))
                                    .font(.caption2).foregroundStyle(.secondary)
                                Text(line.text).font(.caption).foregroundStyle(line.colour)
                            }
                        }
                    }
                }
            }
            .navigationTitle("Settings")
            .onAppear {
                // Seed once, so the field is not overwritten while being typed in.
                if !nameLoaded, let username = store.snapshot?.identity.username {
                    name = username
                    nameLoaded = true
                }
            }
        }
    }
}
