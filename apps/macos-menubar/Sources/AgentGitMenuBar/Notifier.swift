import CryptoKit
import Foundation
import UserNotifications

/// Posts notifications for things only the user can fix. One key notifies at most once per
/// window, across restarts, so a problem that persists does not repeat itself every poll. A key is
/// stamped only once its notification was delivered: one refused or not yet allowed is tried again.
final class Notifier {
    private static let stampsKey = "notifiedAt"
    private static let window: TimeInterval = 6 * 60 * 60
    private let defaults = UserDefaults.standard
    private var asked = false
    /// Keys posted and not yet delivered, so a poll does not post the same one twice meanwhile.
    private var inFlight = Set<String>()

    /// Notifications need an app bundle; `swift run` has none and posts nothing.
    private var available: Bool { Bundle.main.bundleIdentifier != nil }

    /// A stamp key for `kind` and the text it is about. Only a digest of the text is kept, so
    /// the app's preferences never hold a reason, a repository name or a URL.
    static func key(_ kind: String, _ parts: String...) -> String {
        let digest = SHA256.hash(data: Data(parts.joined(separator: "\n").utf8))
        return kind + ":" + digest.prefix(12).map { String(format: "%02x", $0) }.joined()
    }

    /// The part of a reason a notification carries: its first line. A notification can show on
    /// the lock screen, and the hint and continuation lines below it stay in the menu.
    static func firstLine(_ text: String) -> String {
        String(text.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: true).first ?? Substring(text))
    }

    func post(key: String, title: String, body: String) {
        guard available, !inFlight.contains(key) else { return }
        let stamps = defaults.dictionary(forKey: Self.stampsKey) as? [String: Double] ?? [:]
        if let last = stamps[key], Date().timeIntervalSince1970 - last < Self.window { return }
        inFlight.insert(key)

        let center = UNUserNotificationCenter.current()
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        let request = UNNotificationRequest(identifier: key, content: content, trigger: nil)
        let deliver = { [weak self] in
            center.add(request) { error in
                DispatchQueue.main.async { self?.delivered(key, error == nil) }
            }
        }
        if asked { return deliver() }
        asked = true
        center.requestAuthorization(options: [.alert, .sound]) { [weak self] granted, _ in
            if granted { return deliver() }
            DispatchQueue.main.async { self?.delivered(key, false) }
        }
    }

    private func delivered(_ key: String, _ ok: Bool) {
        inFlight.remove(key)
        guard ok else { return }
        let now = Date().timeIntervalSince1970
        var stamps = defaults.dictionary(forKey: Self.stampsKey) as? [String: Double] ?? [:]
        stamps = stamps.filter { now - $0.value < Self.window }
        stamps[key] = now
        defaults.set(stamps, forKey: Self.stampsKey)
    }
}
