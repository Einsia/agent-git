import AppKit
import Foundation

/// What the menu bar icon says at a glance.
enum Health {
    case starting, idle, syncing, attention, paused

    var symbol: String {
        switch self {
        case .starting: return "icloud"
        case .idle: return "checkmark.icloud"
        case .syncing: return "arrow.clockwise.icloud"
        case .attention: return "exclamationmark.icloud"
        case .paused: return "icloud.slash"
        }
    }
}

/// Something the user has to act on, in plain words, with the one action that fixes it.
struct Problem: Identifiable, Equatable {
    enum Fix: Equatable { case signIn, retry(String), chooseCLI, update, none }

    let id: String
    let title: String
    let detail: String?
    let fix: Fix
}

@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var cli: AgitCLI?
    @Published private(set) var version: String?
    @Published private(set) var whoami: Whoami?
    @Published private(set) var signedOutReason: String?
    @Published private(set) var folders: [Folder] = []
    @Published private(set) var listProblem: String?
    @Published private(set) var listUnsupported = false
    @Published private(set) var incomplete = false
    @Published private(set) var refreshing = false
    @Published private(set) var lastRefresh: Date?
    /// Folder path → what the app is running there right now.
    @Published private(set) var busy: [String: String] = [:]
    @Published private(set) var history: [UploadEvent] = []
    /// Folders this app paused, with their destinations, so resuming restores exactly them.
    @Published private(set) var paused: [String: String] = [:]
    /// The outcome of the last action, shown once in the menu.
    @Published var notice: String?

    private var timer: Timer?
    private let notifier = Notifier()
    private var seen: [String: LastUpload] = [:]
    private var wasSignedIn: Bool?
    private var refreshAgain = false
    private var waiting: [String: [Job]] = [:]
    private let defaults = UserDefaults.standard
    private static let historyKey = "uploadHistory"
    private static let pausedKey = "pausedFolders"
    private static let historyLimit = 30
    private static let pollInterval: TimeInterval = 30

    init() {
        if let data = defaults.data(forKey: Self.historyKey),
           let saved = try? JSONDecoder().decode([UploadEvent].self, from: data) {
            history = saved
        }
        paused = defaults.dictionary(forKey: Self.pausedKey) as? [String: String] ?? [:]
        cli = AgitCLI.locate()
        Task {
            await refresh()
            version = await cli?.version()
        }
        timer = Timer.scheduledTimer(withTimeInterval: Self.pollInterval, repeats: true) { [weak self] _ in
            Task { @MainActor in await self?.refresh() }
        }
    }

    // MARK: State

    var signedIn: Bool { whoami?.signedIn == true }

    var problems: [Problem] {
        var problems: [Problem] = []
        if cli == nil {
            problems.append(Problem(
                id: "cli", title: "agit was not found on this Mac",
                detail: "Install it with npx -y create-agit@latest, or choose the agit program.",
                fix: .chooseCLI))
            return problems
        }
        if let signedOutReason {
            problems.append(Problem(
                id: "sign-in", title: "Sign in so conversations can upload",
                detail: signedOutReason, fix: .signIn))
        }
        if listUnsupported {
            problems.append(Problem(
                id: "update", title: "This agit cannot list folders yet",
                detail: "\(version ?? "The installed agit") predates `agit project list`. Update agit.",
                fix: .update))
        } else if let listProblem {
            problems.append(Problem(id: "list", title: "Could not read agit's folders", detail: listProblem, fix: .none))
        }
        for folder in folders where folder.failing {
            problems.append(Problem(
                id: "upload:\(folder.folder)",
                title: "Uploads from \(folder.name) are failing",
                detail: folder.lastUpload?.error, fix: .retry(folder.folder)))
        }
        return problems
    }

    var health: Health {
        if lastRefresh == nil && cli != nil { return .starting }
        if !problems.isEmpty { return .attention }
        if !busy.isEmpty || folders.contains(where: { $0.queued > 0 }) { return .syncing }
        if !folders.contains(where: \.isOn) { return .paused }
        return .idle
    }

    var headline: String {
        switch health {
        case .starting: return "Checking agit…"
        case .attention:
            return problems.count == 1 ? "Needs your attention" : "\(problems.count) things need your attention"
        case .syncing:
            let queued = folders.reduce(0) { $0 + $1.queued }
            if let running = busy.values.first { return running }
            return queued == 1 ? "Uploading a conversation…" : "Uploading \(queued) conversations…"
        case .paused:
            return paused.isEmpty ? "No folder uploads automatically" : "Syncing is paused"
        case .idle: return "Conversations are up to date"
        }
    }

    // MARK: Refresh

    /// A refresh asked for while one runs is folded into one more pass after it, so the state
    /// an action leaves behind is always read.
    func refresh() async {
        guard !refreshing else {
            refreshAgain = true
            return
        }
        refreshing = true
        defer { refreshing = false }
        repeat {
            refreshAgain = false
            if cli == nil { cli = AgitCLI.locate() }
            guard let cli else {
                lastRefresh = Date()
                return
            }
            async let whoamiResult = try? cli.run(["whoami"], timeout: 20)
            async let listResult = try? cli.run(["project", "list"], timeout: 120)
            let (who, list) = await (whoamiResult, listResult)
            applyWhoami(who)
            applyList(list)
            lastRefresh = Date()
        } while refreshAgain
    }

    private func applyWhoami(_ result: CLIResult?) {
        guard let result else { return }
        if result.ok, let value = try? result.decode(Whoami.self) {
            whoami = value
            signedOutReason = value.signedIn ? nil : "Your sign-in to \(Words.host(value.hub ?? "the Hub")) expired."
        } else if result.exitCode == 5 {
            whoami = nil
            signedOutReason = result.reason
        }
        let nowSignedIn = signedOutReason == nil
        if wasSignedIn == true && !nowSignedIn {
            notifier.post(
                key: "sign-in",
                title: "Sign in to AgentGit again",
                body: "Conversations keep saving on this Mac but cannot upload until you sign in.")
        }
        wasSignedIn = nowSignedIn
    }

    private func applyList(_ result: CLIResult?) {
        guard let result else {
            listProblem = CLIError.timedOut.errorDescription
            return
        }
        guard result.ok else {
            listUnsupported = result.kind == "parse_error"
            listProblem = result.reason
            return
        }
        do {
            let list = try result.decode(ProjectList.self)
            listUnsupported = false
            listProblem = nil
            incomplete = list.incomplete
            folders = list.folders.sorted { ($0.isOn ? 0 : 1, $0.name) < ($1.isOn ? 0 : 1, $1.name) }
            observe(list.folders)
        } catch {
            listProblem = CLIError.unreadable(error.localizedDescription).errorDescription
        }
    }

    /// Records each finished upload attempt once, and tells the user about a refused one.
    private func observe(_ folders: [Folder]) {
        for folder in folders {
            guard let last = folder.lastUpload, let at = last.date, seen[folder.folder] != last else { continue }
            seen[folder.folder] = last
            // An upload refused because the repository's automatic publication is off is that
            // choice taking effect; it is neither a failure to report nor an upload.
            if folder.autoPushOff && !last.ok { continue }
            let title = folder.recent.first { $0.sessionId == last.sessionId }?.displayTitle
            let event = UploadEvent(folder: folder.folder, at: at, ok: last.ok, reason: last.error, conversation: title)
            if !history.contains(event) {
                history.append(event)
                history.sort { $0.at > $1.at }
                history = Array(history.prefix(Self.historyLimit))
                if let data = try? JSONEncoder().encode(history) { defaults.set(data, forKey: Self.historyKey) }
            }
            if !last.ok && folder.isOn {
                notifier.post(
                    key: "upload:\(folder.folder):\(last.error ?? "")",
                    title: "Upload refused in \(folder.name)",
                    body: last.error ?? "agit could not upload the last conversation.")
            }
        }
    }

    // MARK: Actions

    /// Uploads the folder's eligible conversations again under its recorded policy.
    func retry(_ path: String) {
        perform(path, "Retrying uploads in \((path as NSString).lastPathComponent)…") { cli in
            try await cli.run(["--yes", "project", "sync", path], timeout: 1800)
        } done: { _ in "Retry finished." }
    }

    /// Turning uploads on publishes this folder's new conversations, so it always asks first.
    func turnOn(_ folder: Folder) {
        guard confirmUpload(folders: [(folder.folder, folder.repository)], includeHistory: false) else { return }
        bind(folder.folder, repository: folder.repository, history: false)
    }

    func turnOff(_ folder: Folder, then: ((Bool) -> Void)? = nil) {
        perform(
            folder.folder, "Turning off uploads in \(folder.name)…",
            { cli in try await cli.run(["project", "unbind", folder.folder]) },
            done: { _ in "\(folder.name) no longer uploads. Its saved history is kept." },
            then: then)
    }

    /// A folder is remembered for resuming only once its uploads are actually off; one that is
    /// busy is paused after its running command.
    func pauseAll() {
        for folder in folders where folder.isOn {
            turnOff(folder) { [weak self] ok in
                guard ok, let self else { return }
                self.paused[folder.folder] = folder.repository
                self.defaults.set(self.paused, forKey: Self.pausedKey)
            }
        }
    }

    func resumeAll() {
        let targets = paused.sorted { $0.key < $1.key }.map { ($0.key, $0.value) }
        guard !targets.isEmpty, confirmUpload(folders: targets, includeHistory: false) else { return }
        for (path, repository) in targets {
            bind(path, repository: repository, history: false) { [weak self] ok in
                guard ok, let self else { return }
                self.paused.removeValue(forKey: path)
                self.defaults.set(self.paused, forKey: Self.pausedKey)
            }
        }
    }

    /// Enrolls a folder the user picked. A missing repository is created private, never public.
    func enroll(path: String, repository: String, includeHistory: Bool, createPrivate: Bool) {
        guard confirmUpload(folders: [(path, repository)], includeHistory: includeHistory) else { return }
        let parts = repository.split(separator: "/", maxSplits: 1).map(String.init)
        let ownName = parts.count == 2 && parts[0] == whoami?.account
        perform(path, "Turning on uploads in \((path as NSString).lastPathComponent)…") { cli in
            if createPrivate && ownName {
                // An existing repository refuses creation; the bind below reports any real problem.
                _ = try? await cli.run(["--yes", "repo", "create", parts[1], "--private"], timeout: 120)
            }
            return try await cli.run(
                ["--yes", "project", "bind", path, "--repo", repository,
                 "--history", includeHistory ? "all" : "none", "--auto-upload"],
                timeout: includeHistory ? 3600 : 600)
        } done: { _ in "\((path as NSString).lastPathComponent) now uploads its new conversations to \(repository)." }
    }

    private func bind(_ path: String, repository: String, history: Bool, then: ((Bool) -> Void)? = nil) {
        let name = (path as NSString).lastPathComponent
        perform(
            path, "Turning on uploads in \(name)…",
            { cli in
                try await cli.run(
                    ["--yes", "project", "bind", path, "--repo", repository,
                     "--history", history ? "all" : "none", "--auto-upload"],
                    timeout: 600)
            },
            done: { _ in "\(name) uploads automatically again." },
            then: then)
    }

    func signIn() {
        perform("sign-in", "Waiting for you to approve the sign-in in your browser…") { cli in
            let start = try await cli.run(["login"], timeout: 60)
            guard let value = start.value,
                  let handoff = try? JSONDecoder.agit.decode(LoginHandoff.self, from: value),
                  let url = URL(string: handoff.authorizationUrl) else { return start }
            await MainActor.run { _ = NSWorkspace.shared.open(url) }
            let complete = Array(handoff.completeCommand.dropFirst()) + ["--wait", "600"]
            return try await cli.run(complete, timeout: 660)
        } done: { _ in "Signed in." }
    }

    func chooseCLI() {
        let panel = NSOpenPanel()
        panel.title = "Choose the agit program"
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".local/bin")
        NSApp.activate(ignoringOtherApps: true)
        guard panel.runModal() == .OK, let url = panel.url else { return }
        defaults.set(url.path, forKey: AgitCLI.pathKey)
        cli = AgitCLI.locate()
        Task {
            version = await cli?.version()
            await refresh()
        }
    }

    func open(_ link: String?) {
        guard let link, let url = URL(string: link) else { return }
        NSWorkspace.shared.open(url)
    }

    func copy(_ text: String, what: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        notice = "Copied \(what)."
    }

    func reveal(_ path: String) {
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    // MARK: Helpers

    private struct Job {
        let description: String
        let work: (AgitCLI) async throws -> CLIResult
        let done: (CLIResult) -> String
        let then: ((Bool) -> Void)?
    }

    /// Commands for one folder run one at a time, in the order asked: a command asked for while
    /// another runs there waits for it instead of being dropped.
    private func perform(
        _ key: String,
        _ description: String,
        _ work: @escaping (AgitCLI) async throws -> CLIResult,
        done: @escaping (CLIResult) -> String,
        then: ((Bool) -> Void)? = nil
    ) {
        let job = Job(description: description, work: work, done: done, then: then)
        if busy[key] != nil {
            waiting[key, default: []].append(job)
        } else {
            start(job, key)
        }
    }

    private func start(_ job: Job, _ key: String) {
        guard let cli else {
            job.then?(false)
            return
        }
        busy[key] = job.description
        notice = nil
        Task {
            var ok = false
            do {
                let result = try await job.work(cli)
                ok = result.ok
                notice = result.ok ? job.done(result) : result.reason
            } catch {
                notice = error.localizedDescription
            }
            job.then?(ok)
            if let next = waiting[key]?.first {
                waiting[key]?.removeFirst()
                if waiting[key]?.isEmpty == true { waiting.removeValue(forKey: key) }
                start(next, key)
            } else {
                busy.removeValue(forKey: key)
            }
            await refresh()
        }
    }

    /// Publication needs a yes from the user every time it is turned on; nothing here uploads
    /// silently.
    private func confirmUpload(folders targets: [(String, String)], includeHistory: Bool) -> Bool {
        let alert = NSAlert()
        let hub = Words.host(whoami?.hub ?? "the Hub")
        let account = whoami?.account ?? "your account"
        if targets.count == 1, let (path, repository) = targets.first {
            alert.messageText = "Upload conversations from \((path as NSString).lastPathComponent)?"
            let which = includeHistory
                ? "Claude Code and Codex conversations that already ran in \(Words.tilde(path)), and every new one there,"
                : "New Claude Code and Codex conversations in \(Words.tilde(path))"
            alert.informativeText = """
            \(which) will upload to \(repository) on \(hub) as \(account) after each turn.

            This also turns on automatic upload for every conversation this Mac saves in \
            \(repository). Everyone who can open \(repository) on \(hub) can read them; agit checks \
            every upload for secrets first.
            """
        } else {
            alert.messageText = "Resume uploads in \(targets.count) folders?"
            alert.informativeText = targets
                .map { "\(Words.tilde($0.0)) → \($0.1)" }
                .joined(separator: "\n")
                + "\n\nNew conversations there upload to \(hub) as \(account) after each turn, and "
                + "automatic upload turns on for every conversation this Mac saves in those repositories."
        }
        alert.addButton(withTitle: "Turn on uploads")
        alert.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        return alert.runModal() == .alertFirstButtonReturn
    }
}

/// The browser sign-in agit starts when it cannot ask in a terminal.
private struct LoginHandoff: Decodable {
    let authorizationUrl: String
    let completeCommand: [String]
}
