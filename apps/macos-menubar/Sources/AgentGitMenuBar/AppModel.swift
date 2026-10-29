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
    enum Fix: Equatable { case signIn, retry(String), allowAutoUpload(String), chooseCLI, none }

    let id: String
    let title: String
    let detail: String?
    let fix: Fix
}

/// What `agit setup --hooks --installed-only` reports.
private struct HookRepair: Decodable {
    let hookFilesRepaired: Int
}

/// The hook commands in Claude Code's settings file, read without running anything.
private struct ClaudeHooks {
    /// The file `agit setup` installs Claude Code's hooks into.
    static var settings: URL {
        if let dir = ProcessInfo.processInfo.environment["CLAUDE_CONFIG_DIR"], !dir.isEmpty {
            return URL(fileURLWithPath: dir).appendingPathComponent("settings.json")
        }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".claude/settings.json")
    }

    /// Event → the command of every command hook under it.
    let commands: [String: [String]]

    init?(_ url: URL) {
        guard let data = try? Data(contentsOf: url),
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let hooks = root["hooks"] as? [String: Any]
        else { return nil }
        var commands: [String: [String]] = [:]
        for (event, groups) in hooks {
            for group in groups as? [[String: Any]] ?? [] {
                for entry in group["hooks"] as? [[String: Any]] ?? [] {
                    if let command = entry["command"] as? String { commands[event, default: []].append(command) }
                }
            }
        }
        self.commands = commands
    }

    /// Whether `program` settles Claude Code's turns here and every agit capture hook here runs
    /// `program`, written the way `agit setup` writes it. `agit setup --hooks` points every agit
    /// hook it finds at the agit that runs it, so a capture hook of another program would move.
    func capturesOnly(with program: String) -> Bool {
        let words = [AgitCLI.shellWord(program), program]
        let runs = { (command: String, verb: String) in words.contains { command.hasPrefix("\($0) hooks \(verb)") } }
        let captures = commands.values.joined().filter { $0.contains(" hooks ingest") || $0.contains(" hooks settle") }
        return commands["Stop", default: []].contains { runs($0, "settle") }
            && captures.allSatisfy { runs($0, "ingest") || runs($0, "settle") }
    }

    var answersPermissions: Bool {
        commands["PermissionRequest", default: []].contains { $0.contains(" hooks native-permission") }
    }
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
    /// The agit that lists folders when the chosen one predates the folder list.
    private var lister: AgitCLI?
    /// `AgitCLI.stamp` of the chosen agit `lister` was found for.
    private var listerChosenFor: String?
    /// `AgitCLI.stamp` of the program `version` was read from.
    private var versionStamp: String?
    /// When a hook repair that has not finished was last tried.
    private var repairTried: Date?
    /// `AgitCLI.stamp` of a program found not to have the permission hook.
    private var lacksPermissionHook: String?
    private let defaults = UserDefaults.standard
    private static let historyKey = "uploadHistory"
    private static let pausedKey = "pausedFolders"
    private static let hooksRepairedKey = "hooksRepairedFor"
    private static let permissionHookAskedKey = "permissionHookAskedFor"
    private static let repairRetry: TimeInterval = 600
    private static let historyLimit = 30
    private static let pollInterval: TimeInterval = 30

    init() {
        if let data = defaults.data(forKey: Self.historyKey),
           let saved = try? JSONDecoder().decode([UploadEvent].self, from: data) {
            history = saved
        }
        paused = defaults.dictionary(forKey: Self.pausedKey) as? [String: String] ?? [:]
        cli = AgitCLI.locate()
        Task { await refresh() }
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
            // No released agit may list folders yet, so this names the release that does rather
            // than asking for an update that would not help.
            problems.append(Problem(
                id: "list-unsupported", title: "This agit doesn't list folders yet",
                detail: "\(version ?? "The installed agit") predates the folder list, which comes with "
                    + "agit \(AgitCLI.listingVersion).",
                fix: .chooseCLI))
        } else if let listProblem {
            problems.append(Problem(id: "list", title: "Could not read agit's folders", detail: listProblem, fix: .none))
        }
        for folder in folders where folder.failing {
            problems.append(Problem(
                id: "upload:\(folder.folder)",
                title: "Uploads from \(folder.name) are failing",
                detail: folder.lastUpload?.error, fix: .retry(folder.folder)))
        }
        for folder in folders where folder.blocked {
            problems.append(Problem(
                id: "blocked:\(folder.folder)",
                title: "Uploads from \(folder.name) are blocked",
                detail: folder.blockedReason, fix: .allowAutoUpload(folder.folder)))
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
            // A reader found for an earlier chosen agit is not kept for a new one, which may list.
            if listerChosenFor != cli.stamp { lister = nil }
            let listing = lister ?? cli
            async let whoamiResult = try? cli.run(["whoami"], timeout: 20)
            async let listResult = try? listing.run(["project", "list"], timeout: 120)
            var (who, list) = await (whoamiResult, listResult)
            // A reader that no longer starts or lists is dropped, and the chosen agit is asked
            // first again: keeping it would retry a moved or deleted program on every poll.
            if lister != nil, list == nil || list?.kind == "parse_error" {
                lister = nil
                list = try? await cli.run(["project", "list"], timeout: 120)
            }
            if list?.kind == "parse_error" {
                if let found = await listingAlternative(to: cli) {
                    listerChosenFor = cli.stamp
                    lister = found.cli
                    list = found.result
                }
            }
            applyWhoami(who)
            applyList(list)
            lastRefresh = Date()
            if cli.stamp != versionStamp { await readVersion(cli) }
            await repairHooks(cli)
            await offerPermissionHook(cli)
        } while refreshAgain
    }

    /// An installed agit that lists folders, with its answer, for a chosen agit that predates the
    /// folder list. Only the list is read through it; every action still runs the chosen agit.
    private func listingAlternative(to cli: AgitCLI) async -> (cli: AgitCLI, result: CLIResult)? {
        func lists(_ candidate: AgitCLI) async -> CLIResult? {
            guard let result = try? await candidate.run(["project", "list"], timeout: 120),
                  result.kind != "parse_error" else { return nil }
            return result
        }
        for alternative in cli.alternatives() {
            if let result = await lists(alternative) { return (alternative, result) }
        }
        if let alternative = await cli.shellAlternative(), let result = await lists(alternative) {
            return (alternative, result)
        }
        return nil
    }

    /// Reads `agit --version`. A read that fails leaves the stamp unset, so the next poll reads
    /// again.
    private func readVersion(_ cli: AgitCLI) async {
        let stamp = cli.stamp
        version = await cli.version()
        guard version != nil else {
            versionStamp = nil
            return
        }
        versionStamp = stamp
        repairTried = nil
    }

    /// Repairs agit's hooks once for each program path and version. A hook keeps the program path
    /// it was installed with, so a hook that no longer starts is repaired only by an agit run
    /// outside the hooks; `--installed-only` rewrites just the broken command text of hooks that
    /// already exist and installs none, so no runtime gains a hook it was never given. A repair
    /// that does not finish is tried again on a later poll, at most once per `repairRetry`. What
    /// it repaired is announced once, because until then nothing was being saved, and a repair
    /// that left a hook file broken says so instead of saying all is well.
    private func repairHooks(_ cli: AgitCLI) async {
        guard let version, versionStamp == cli.stamp else { return }
        let key = "\(cli.executable.path)\n\(version)"
        guard defaults.string(forKey: Self.hooksRepairedKey) != key else { return }
        if let tried = repairTried, Date().timeIntervalSince(tried) < Self.repairRetry { return }
        repairTried = Date()
        guard let result = try? await cli.run(["setup", "--hooks", "--installed-only"], timeout: 60)
        else { return }
        let repaired = (try? result.decode(HookRepair.self))?.hookFilesRepaired ?? 0
        if repaired > 0 && result.ok {
            notifier.post(
                key: "hooks-repaired",
                title: "Conversations save again",
                body: "AgentGit's hooks could not start, so conversations were not saved. They are repaired; "
                    + "the previous settings are kept beside them with .agit-backup added to the name.")
        } else if repaired > 0 {
            notifier.post(
                key: "hooks-partly-repaired",
                title: "Some conversations still cannot be saved",
                body: "AgentGit repaired some of its hooks, but not all of them. \(result.reason)")
        }
        if result.ok {
            defaults.set(key, forKey: Self.hooksRepairedKey)
            repairTried = nil
        }
    }

    /// Offers, once for each settings file, to add the Claude Code hook that lets approvals
    /// answered in AgentGit decide its permission requests. Where Claude Code's security default is
    /// in force it keeps plugins from seeing those requests, and only this hook carries the answer.
    /// The offer comes only where this agit already settles Claude Code's turns and runs every agit
    /// capture hook, and only from an agit that has the hook, so adding it turns capture on nowhere
    /// new and moves no hook to another program.
    private func offerPermissionHook(_ cli: AgitCLI) async {
        let settings = ClaudeHooks.settings
        var asked = defaults.stringArray(forKey: Self.permissionHookAskedKey) ?? []
        guard !asked.contains(settings.path), lacksPermissionHook != cli.stamp,
              let hooks = ClaudeHooks(settings),
              hooks.capturesOnly(with: cli.executable.path), !hooks.answersPermissions
        else { return }
        guard await cli.knows(["hooks", "native-permission"]) else {
            lacksPermissionHook = cli.stamp
            return
        }
        asked.append(settings.path)
        defaults.set(asked, forKey: Self.permissionHookAskedKey)
        guard confirmPermissionHook(settings) else { return }
        let result = try? await cli.run(["setup", "--hooks", "--runtime", "claude-code"], timeout: 60)
        if let result, result.ok, ClaudeHooks(settings)?.answersPermissions == true {
            notifier.post(
                key: "permission-hook",
                title: "Claude Code's permission requests reach AgentGit",
                body: "Approvals you answer in AgentGit now allow or deny them on this computer.")
        } else {
            let reason = result?.reason ?? "agit did not finish."
            notifier.post(
                key: "permission-hook",
                title: "AgentGit could not add the permission hook",
                body: "\(reason) Running agit setup in a terminal adds it.")
        }
    }

    /// Cancel is the default: once added, an answer given in AgentGit decides for this computer.
    private func confirmPermissionHook(_ settings: URL) -> Bool {
        let alert = NSAlert()
        alert.messageText = "Answer Claude Code's permission requests from AgentGit?"
        alert.informativeText = """
        This lets approvals you answer in AgentGit allow or deny Claude Code's permission requests \
        on this computer. Claude Code still shows its own dialog, and answering there works as before.

        agit rewrites \(Words.tilde(settings.path)) to add it, which can change the order of its \
        settings. The file as it was before AgentGit first changed it stays beside it, with \
        .agit-backup added to the name.
        """
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Add")
        NSApp.activate(ignoringOtherApps: true)
        return alert.runModal() == .alertSecondButtonReturn
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
            // setting taking effect; the folder reads as blocked instead of reporting a failure.
            if folder.blocked && !last.ok { continue }
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
        if folder.blocked { return allowAutoUpload(folder.folder) }
        guard confirmUpload(folders: [(folder.folder, folder.repository)], includeHistory: false) else { return }
        bind(folder.folder, repository: folder.repository, history: false)
    }

    /// A blocked folder's policy already uploads; only its repository's `push.auto` holds it
    /// back, and turning that on is the change the confirmation names.
    func allowAutoUpload(_ path: String) {
        guard let folder = folders.first(where: { $0.folder == path }),
              confirmUpload(folders: [(folder.folder, folder.repository)], includeHistory: false)
        else { return }
        perform(
            path, "Turning on automatic upload for \(folder.repository)…",
            { cli in try await cli.run(["config", "--repo", folder.repository, "push.auto", "true"]) },
            done: { _ in "\(folder.repository) uploads automatically on this Mac again." })
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
        let off = autoPushOff(repository)
        perform(path, "Turning on uploads in \((path as NSString).lastPathComponent)…") { cli in
            if createPrivate && ownName {
                // An existing repository refuses creation; the bind below reports any real problem.
                _ = try? await cli.run(["--yes", "repo", "create", parts[1], "--private"], timeout: 120)
            }
            if off, let refused = try await Self.allowAutoPush(cli, repository) { return refused }
            return try await cli.run(
                ["--yes", "project", "bind", path, "--repo", repository,
                 "--history", includeHistory ? "all" : "none", "--auto-upload"],
                timeout: includeHistory ? 3600 : 600)
        } done: { _ in "\((path as NSString).lastPathComponent) now uploads its new conversations to \(repository)." }
    }

    private func bind(_ path: String, repository: String, history: Bool, then: ((Bool) -> Void)? = nil) {
        let name = (path as NSString).lastPathComponent
        let off = autoPushOff(repository)
        perform(
            path, "Turning on uploads in \(name)…",
            { cli in
                if off, let refused = try await Self.allowAutoPush(cli, repository) { return refused }
                return try await cli.run(
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
        lister = nil
        Task { await refresh() }
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

    /// Whether the listing reports the repository's automatic upload off on this Mac.
    private func autoPushOff(_ repository: String) -> Bool {
        folders.contains { $0.repository == repository && $0.autoPush == false }
    }

    /// Lifts a repository's automatic upload by name, once the person has agreed to exactly that:
    /// agit's bind never lifts an explicit off by itself. The refusal, when there is one, is what
    /// the action reports.
    private static func allowAutoPush(_ cli: AgitCLI, _ repository: String) async throws -> CLIResult? {
        let result = try await cli.run(["config", "--repo", repository, "push.auto", "true"])
        return result.ok ? nil : result
    }

    /// Publication needs a yes from the user every time it is turned on; nothing here uploads
    /// silently. A repository whose `push.auto` is off on this Mac was turned off on purpose, so
    /// the question names that setting, how much it releases, and who can read it, and Cancel is
    /// the default answer.
    private func confirmUpload(folders targets: [(String, String)], includeHistory: Bool) -> Bool {
        let alert = NSAlert()
        let hub = Words.host(whoami?.hub ?? "the Hub")
        let account = whoami?.account ?? "your account"
        let held = Set(targets.map(\.1)).filter(autoPushOff)
        let waiting = folders
            .filter { held.contains($0.repository) }
            .reduce(0) { $0 + ($1.awaitingUpload ?? 0) }
        if targets.count == 1, let (path, repository) = targets.first, held.contains(repository) {
            alert.messageText = "Turn on automatic upload for \(repository)?"
            let release = waiting == 0
                ? ""
                : waiting == 1
                    ? ", including 1 already saved here that has not been uploaded"
                    : ", including \(waiting) already saved here that have not been uploaded"
            alert.informativeText = """
            Automatic upload (push.auto) is off for \(repository) on this Mac. Turning it on \
            uploads every conversation this Mac saves in \(repository), from \(Words.tilde(path)) \
            and anywhere else, after each turn\(release).

            They upload to \(hub) as \(account). Everyone who can open \(repository) there can \
            read them; agit checks every upload for secrets first.
            """
        } else if targets.count == 1, let (path, repository) = targets.first {
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
                + (held.isEmpty ? "" : " It is off on this Mac now for \(held.sorted().joined(separator: ", ")).")
        }
        alert.addButton(withTitle: "Turn on uploads")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].keyEquivalent = ""
        alert.buttons[1].keyEquivalent = "\r"
        NSApp.activate(ignoringOtherApps: true)
        return alert.runModal() == .alertFirstButtonReturn
    }
}

/// The browser sign-in agit starts when it cannot ask in a terminal.
private struct LoginHandoff: Decodable {
    let authorizationUrl: String
    let completeCommand: [String]
}
