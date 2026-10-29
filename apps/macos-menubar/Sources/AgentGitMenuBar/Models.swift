import Foundation

/// `agit whoami --json`. Only token states and expiry times are read; agit never prints token
/// values there, and this app never asks for them.
struct Whoami: Decodable {
    let account: String?
    let email: String?
    let hub: String?
    let tokens: Tokens?

    struct Tokens: Decodable {
        let access: Token?
        let refresh: Token?
    }

    struct Token: Decodable {
        let state: String
        let expiresAt: String?
    }

    /// The sign-in can still renew itself; an expired access token alone is renewed by agit.
    var signedIn: Bool {
        account != nil && (tokens?.refresh?.state ?? "valid") == "valid"
    }
}

/// `agit project list --json`.
struct ProjectList: Decodable {
    let schemaVersion: Int
    let hub: String
    let folders: [Folder]
    let incomplete: Bool
}

struct Folder: Decodable, Identifiable, Equatable {
    /// `on`, `blocked`, `off` or `local_only`.
    let state: String
    let folder: String
    let repository: String
    let repositoryUrl: String?
    /// The repository's automatic publication preference on this Mac; null without a checkout.
    let autoPush: Bool?
    let queued: Int
    let lastUpload: LastUpload?
    let sessionsHere: Int
    /// Conversations here that are saved and not uploaded at their saved head; absent from an
    /// agit that predates the count.
    let awaitingUpload: Int?
    let recent: [Conversation]

    var id: String { folder }
    var isOn: Bool { state == "on" }
    var name: String { (folder as NSString).lastPathComponent }
    var path: String { Words.tilde(folder) }
    var failing: Bool { isOn && lastUpload?.ok == false }
    /// The folder is set to upload, but its repository's automatic publication is off on this
    /// Mac, so its conversations stay here until someone turns that on.
    var blocked: Bool { state == "blocked" }
    var description: String { Words.folderState(state) }
    /// Why nothing uploads, in the words `agit project list` uses.
    var blockedReason: String {
        let waiting = awaitingUpload ?? 0
        let held = waiting == 0
            ? ""
            : waiting == 1
                ? " 1 conversation here is saved but not uploaded."
                : " \(waiting) conversations here are saved but not uploaded."
        return "Automatic upload (push.auto) is off for \(repository) on this Mac." + held
    }
    var detail: String {
        blocked ? "\(path)\n\(blockedReason) Turn it on with: \(autoPushCommand)" : path
    }
    var autoPushCommand: String { "agit config --repo \(repository) push.auto true" }
}

struct LastUpload: Decodable, Equatable {
    let at: String?
    let ok: Bool
    let error: String?
    let sessionId: String?

    var date: Date? { at.flatMap(Words.date) }
}

struct Conversation: Decodable, Identifiable, Equatable {
    let title: String?
    let runtime: String
    let sessionId: String
    let lastActive: String
    /// `not_saved`, `not_uploaded`, `waiting` or `uploaded`.
    let status: String
    let repository: String?
    let savedAt: String?
    let url: String?

    var id: String { "\(runtime):\(sessionId)" }
    var displayTitle: String { title ?? "Untitled \(Words.runtime(runtime)) conversation" }
    var lastActiveDate: Date? { Words.date(lastActive) }
}

/// An upload attempt this app saw finish, kept so the menu can list recent uploads; agit itself
/// records only a folder's last result.
struct UploadEvent: Codable, Identifiable, Equatable {
    let folder: String
    let at: Date
    let ok: Bool
    let reason: String?
    let conversation: String?

    var id: String { "\(folder)@\(at.timeIntervalSince1970)" }
}

enum Words {
    static func runtime(_ runtime: String) -> String {
        switch runtime {
        case "claude-code": return "Claude Code"
        case "codex": return "Codex"
        default: return runtime
        }
    }

    static func status(_ status: String) -> String {
        switch status {
        case "uploaded": return "Uploaded"
        case "waiting": return "Newer turns waiting to upload"
        case "not_uploaded": return "Saved on this Mac, not uploaded"
        default: return "Not saved"
        }
    }

    static func folderState(_ state: String) -> String {
        switch state {
        case "on": return "Uploads automatically"
        case "blocked": return "Upload blocked"
        case "off": return "Paused"
        default: return "Saved on this Mac only"
        }
    }

    static func tilde(_ path: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
    }

    static func host(_ url: String) -> String {
        URL(string: url)?.host ?? url
    }

    private static let fractional: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter
    }()

    private static let whole = ISO8601DateFormatter()

    static func date(_ text: String) -> Date? {
        fractional.date(from: text) ?? whole.date(from: text)
    }

    private static let relative: RelativeDateTimeFormatter = {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .full
        return formatter
    }()

    static func ago(_ date: Date?) -> String {
        guard let date else { return "" }
        if Date().timeIntervalSince(date) < 60 { return "just now" }
        return relative.localizedString(for: date, relativeTo: Date())
    }
}
