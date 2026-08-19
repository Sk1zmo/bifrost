// Bifrost for iOS — native SwiftUI over the same Rust core the desktop runs.
//
// The Flutter client in `clients/flutter` also builds for iOS from one
// codebase shared with Android. This target exists for anyone who would
// rather ship a purely native app; both link the identical static library and
// speak the identical protocol.

import SwiftUI

@main
struct BifrostApp: App {
    @StateObject private var store = NodeStore()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ContentView().environmentObject(store)
        }
        .onChange(of: scenePhase) { _, phase in
            // iOS suspends an inactive app's sockets, so beacons would stop
            // and peers would drop this device. Refreshing on the way back in
            // re-syncs immediately rather than waiting out a poll interval.
            if phase == .active {
                store.refresh()
            }
        }
    }
}
