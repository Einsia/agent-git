import Foundation
import UserNotifications

/// Posts notifications for things only the user can fix. One key notifies at most once per
/// window, across restarts, so a problem that persists does not repeat itself every poll.
final class Notifier {
    private static let stampsKey = "notifiedAt"
    private static let window: TimeInterval = 6 * 60 * 60
    private let defaults = UserDefaults.standard
    private var asked = false

    /// Notifications need an app bundle; `swift run` has none and posts nothing.
    private var available: Bool { Bundle.main.bundleIdentifier != nil }

    func post(key: String, title: String, body: String) {
        guard available else { return }
        var stamps = defaults.dictionary(forKey: Self.stampsKey) as? [String: Double] ?? [:]
        let now = Date().timeIntervalSince1970
        if let last = stamps[key], now - last < Self.window { return }
        stamps = stamps.filter { now - $0.value < Self.window }
        stamps[key] = now
        defaults.set(stamps, forKey: Self.stampsKey)

        let center = UNUserNotificationCenter.current()
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        let request = UNNotificationRequest(identifier: key, content: content, trigger: nil)
        if asked {
            center.add(request)
            return
        }
        asked = true
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            if granted { center.add(request) }
        }
    }
}
