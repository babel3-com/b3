import SwiftUI
import GoogleSignIn

@main
struct Babel3App: App {
    init() {
        BroadcastMonitor.shared.startMonitoring()
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .onOpenURL { url in
                    GIDSignIn.sharedInstance.handle(url)
                }
        }
    }
}
