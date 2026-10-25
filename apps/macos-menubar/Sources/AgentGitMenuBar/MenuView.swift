import AppKit
import SwiftUI

struct MenuView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.openWindow) private var openWindow
    @StateObject private var contentHeight = Measured()

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    ForEach(model.problems) { ProblemRow(problem: $0) }
                    if let notice = model.notice {
                        NoticeRow(text: notice) { model.notice = nil }
                    }
                    folders
                    if !model.history.isEmpty { uploads }
                }
                .padding(14)
                .background(GeometryReader { proxy in
                    Color.clear.preference(key: ContentHeight.self, value: proxy.size.height)
                })
            }
            // A scroll view proposes no height of its own, so the menu would collapse it; it takes
            // its content's height up to a limit and scrolls beyond that.
            .frame(height: min(contentHeight.value, 520))
            .onPreferenceChange(ContentHeight.self) { contentHeight.value = $0 }
            Divider()
            footer
        }
        .frame(width: 380)
        .task { await model.refresh() }
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: model.health.symbol)
                .font(.system(size: 20))
                .foregroundStyle(model.health == .attention ? Color.orange : Color.secondary)
                .frame(width: 26)
            VStack(alignment: .leading, spacing: 2) {
                Text(model.headline).font(.system(size: 13, weight: .semibold))
                Text(accountLine).font(.system(size: 11)).foregroundStyle(.secondary)
            }
            Spacer()
            Button {
                Task { await model.refresh() }
            } label: {
                if model.refreshing {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: "arrow.clockwise")
                }
            }
            .buttonStyle(.borderless)
            .help("Check again")
        }
        .padding(14)
    }

    private var accountLine: String {
        guard let whoami = model.whoami, let account = whoami.account else {
            return model.cli == nil ? "agit is not set up" : "Not signed in"
        }
        let hub = Words.host(whoami.hub ?? "")
        let checked = model.lastRefresh.map { " · checked \(Words.ago($0))" } ?? ""
        return "\(account) on \(hub)\(checked)"
    }

    @ViewBuilder private var folders: some View {
        SectionHeader(title: "Folders") {
            Button("Add a folder…") { show("enroll") }
                .buttonStyle(.borderless)
                .font(.system(size: 11))
        }
        if model.folders.isEmpty && model.lastRefresh != nil && model.problems.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("No folder uploads its conversations yet.")
                    .font(.system(size: 12, weight: .medium))
                Text("Turn a project folder on once, and every Claude Code and Codex conversation in it saves after each turn and uploads to your repository.")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        ForEach(model.folders) { FolderRow(folder: $0) }
        if model.incomplete {
            Text("Some of agit's records could not be read; this list may be missing a folder.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var uploads: some View {
        SectionHeader(title: "Recent uploads") { EmptyView() }
        ForEach(model.history.prefix(5)) { UploadRow(event: $0) }
    }

    private var footer: some View {
        HStack(spacing: 14) {
            Button("What agit can do") { show("guide") }
            if !model.paused.isEmpty && !model.folders.contains(where: \.isOn) {
                Button("Resume syncing") { model.resumeAll() }
            } else if model.folders.contains(where: \.isOn) {
                Button("Pause syncing") { model.pauseAll() }
            }
            Spacer()
            Menu {
                if let version = model.version { Text(version) }
                if let path = model.cli?.executable.path { Text(Words.tilde(path)) }
                Button("Choose the agit program…") { model.chooseCLI() }
                Divider()
                Button("Quit AgentGit") { NSApp.terminate(nil) }
            } label: {
                Image(systemName: "gearshape")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
        }
        .buttonStyle(.borderless)
        .font(.system(size: 12))
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    private func show(_ id: String) {
        openWindow(id: id)
        NSApp.activate(ignoringOtherApps: true)
    }
}

private struct ContentHeight: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = max(value, nextValue()) }
}

private struct SectionHeader<Trailing: View>: View {
    let title: String
    @ViewBuilder let trailing: () -> Trailing

    var body: some View {
        HStack {
            Text(title.uppercased())
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(.secondary)
            Spacer()
            trailing()
        }
    }
}

private struct ProblemRow: View {
    @EnvironmentObject private var model: AppModel
    let problem: Problem

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 4) {
                Text(problem.title).font(.system(size: 12, weight: .semibold))
                if let detail = problem.detail {
                    Text(detail)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(4)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                fix
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.orange.opacity(0.08)))
    }

    @ViewBuilder private var fix: some View {
        switch problem.fix {
        case .signIn:
            Button("Sign in…") { model.signIn() }.disabled(model.busy["sign-in"] != nil)
        case .retry(let path):
            if let running = model.busy[path] {
                Label(running, systemImage: "hourglass").font(.system(size: 11)).foregroundStyle(.secondary)
            } else {
                Button("Retry uploads") { model.retry(path) }
            }
        case .chooseCLI:
            Button("Choose the agit program…") { model.chooseCLI() }
        case .update:
            Button("Copy the update command") { model.copy("agit upgrade", what: "the update command") }
        case .none:
            EmptyView()
        }
    }
}

private struct NoticeRow: View {
    let text: String
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .top) {
            Text(text)
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            Spacer()
            Button(action: dismiss) { Image(systemName: "xmark") }.buttonStyle(.borderless)
        }
    }
}

private struct FolderRow: View {
    @EnvironmentObject private var model: AppModel
    let folder: Folder
    @StateObject private var expanded: Flag

    /// Folders that only save on this Mac start folded, so the ones that upload, or would but
    /// have upload turned off, stay in view.
    init(folder: Folder) {
        self.folder = folder
        _expanded = StateObject(wrappedValue: Flag(folder.state != "local_only" || folder.autoPushOff))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Button {
                    expanded.on.toggle()
                } label: {
                    Image(systemName: expanded.on ? "chevron.down" : "chevron.right")
                        .font(.system(size: 9, weight: .semibold))
                        .frame(width: 10)
                }
                .buttonStyle(.borderless)
                Image(systemName: "folder").foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(folder.name).font(.system(size: 12, weight: .semibold))
                    Text("\(folder.description) · \(folder.repository)")
                        .font(.system(size: 11))
                        .foregroundStyle(folder.failing ? Color.orange : Color.secondary)
                        .lineLimit(1)
                }
                .help(folder.detail)
                Spacer()
                Toggle("", isOn: Binding(
                    get: { folder.isOn },
                    set: { $0 ? model.turnOn(folder) : model.turnOff(folder) }))
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .labelsHidden()
                    .disabled(model.busy[folder.folder] != nil)
                    .help(folder.isOn ? "Stop uploading this folder's conversations" : "Upload this folder's new conversations")
                Menu {
                    Button("Open the repository on the Hub") { model.open(folder.repositoryUrl) }
                        .disabled(folder.repositoryUrl == nil)
                    if let link = folder.repositoryUrl {
                        Button("Copy the repository link") { model.copy(link, what: "the repository link") }
                    }
                    if folder.autoPushOff {
                        Button("Copy the command that turns automatic upload on") {
                            model.copy(folder.autoPushCommand, what: "the command")
                        }
                    }
                    if folder.isOn {
                        Button("Upload again now") { model.retry(folder.folder) }
                    }
                    Divider()
                    Button("Show in Finder") { model.reveal(folder.folder) }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
            }
            if let running = model.busy[folder.folder] {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text(running).font(.system(size: 11)).foregroundStyle(.secondary)
                }
                .padding(.leading, 18)
            } else if folder.queued > 0 {
                Text(folder.queued == 1 ? "Uploading a conversation…" : "Uploading \(folder.queued) conversations…")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .padding(.leading, 18)
            }
            if expanded.on {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(folder.recent) { ConversationRow(conversation: $0, folder: folder) }
                    if folder.sessionsHere > folder.recent.count {
                        let earlier = folder.sessionsHere - folder.recent.count
                        Text(earlier == 1 ? "and 1 earlier conversation" : "and \(earlier) earlier conversations")
                            .font(.system(size: 11))
                            .foregroundStyle(.tertiary)
                    }
                    if folder.sessionsHere == 0 {
                        Text("No Claude Code or Codex conversation has run here yet.")
                            .font(.system(size: 11))
                            .foregroundStyle(.tertiary)
                    }
                }
                .padding(.leading, 18)
            }
        }
    }
}

private struct ConversationRow: View {
    @EnvironmentObject private var model: AppModel
    let conversation: Conversation
    let folder: Folder
    @StateObject private var hovering = Flag(false)

    private var failed: Bool {
        folder.failing && folder.lastUpload?.sessionId == conversation.sessionId
            && conversation.status != "uploaded"
    }

    var body: some View {
        HStack(spacing: 6) {
            Text(conversation.runtime == "codex" ? "Codex" : "Claude")
                .font(.system(size: 9, weight: .medium))
                .padding(.horizontal, 4)
                .padding(.vertical, 1)
                .background(RoundedRectangle(cornerRadius: 3).stroke(Color.secondary.opacity(0.4)))
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 0) {
                Text(conversation.displayTitle)
                    .font(.system(size: 12))
                    .lineLimit(1)
                    .truncationMode(.tail)
                Text(subtitle)
                    .font(.system(size: 10))
                    .foregroundStyle(failed ? Color.orange : Color.secondary)
                    .lineLimit(1)
            }
            .help(conversation.displayTitle)
            Spacer(minLength: 4)
            if let url = conversation.url {
                Button { model.open(url) } label: { Image(systemName: "arrow.up.forward.square") }
                    .help("Open this conversation on the Hub")
                    .opacity(hovering.on ? 1 : 0.35)
                Button { model.copy(url, what: "the link to this conversation") } label: { Image(systemName: "link") }
                    .help("Copy a link to this conversation")
                    .opacity(hovering.on ? 1 : 0.35)
            }
        }
        .buttonStyle(.borderless)
        .onHover { hovering.on = $0 }
    }

    private var subtitle: String {
        let status = failed ? "Upload failed" : Words.status(conversation.status)
        return "\(Words.ago(conversation.lastActiveDate)) · \(status)"
    }
}

private struct UploadRow: View {
    let event: UploadEvent

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: event.ok ? "checkmark.circle" : "xmark.circle")
                .foregroundStyle(event.ok ? Color.green : Color.orange)
            VStack(alignment: .leading, spacing: 1) {
                Text(event.ok ? "Uploaded \(event.conversation.map { "\"\($0)\"" } ?? "conversations")" : "Upload failed")
                    .font(.system(size: 12))
                    .lineLimit(1)
                Text("\((event.folder as NSString).lastPathComponent) · \(Words.ago(event.at))")
                    .font(.system(size: 10))
                    .foregroundStyle(.secondary)
                if let reason = event.reason, !event.ok {
                    Text(reason)
                        .font(.system(size: 10))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .textSelection(.enabled)
                }
            }
        }
    }
}
