import AppKit
import SwiftUI

@main
struct AgentGitMenuBarApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate

    var body: some Scene {
        MenuBarExtra {
            MenuView().environmentObject(delegate.model)
        } label: {
            MenuBarLabel(model: delegate.model)
        }
        .menuBarExtraStyle(.window)

        Window("What agit can do", id: "guide") {
            GuideView().environmentObject(delegate.model)
        }
        .windowResizability(.contentSize)

        Window("Turn on uploads", id: "enroll") {
            EnrollView().environmentObject(delegate.model)
        }
        .windowResizability(.contentSize)
    }
}

private struct MenuBarLabel: View {
    @ObservedObject var model: AppModel

    var body: some View {
        Image(systemName: model.health.symbol)
    }
}

/// The app lives in the menu bar only. The bundle's `LSUIElement` keeps it out of the Dock;
/// this covers a run without the bundle, such as `swift run`.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = AppModel()
    private var preview: NSWindow?

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }

    /// `AGIT_MENUBAR_OPEN=menu|guide|enroll` also shows that view in an ordinary window at launch,
    /// for screenshots and UI checks: the system draws status items itself, so the menu cannot be
    /// opened from inside the app, and a crowded menu bar can hide its icon.
    func applicationDidFinishLaunching(_ notification: Notification) {
        let content: AnyView
        switch ProcessInfo.processInfo.environment["AGIT_MENUBAR_OPEN"] {
        case "menu": content = AnyView(MenuView())
        case "guide": content = AnyView(GuideView())
        case "enroll": content = AnyView(EnrollView())
        default: return
        }
        let window = NSWindow(contentViewController: NSHostingController(rootView: content.environmentObject(model)))
        window.title = "AgentGit"
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        preview = window
    }
}
