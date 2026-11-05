import AppKit
import SwiftUI

/// Turns uploads on for one folder. Nothing is enrolled until the user confirms the destination
/// in the alert that `AppModel.enroll` shows.
struct EnrollView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss
    /// The window stays open while a folder is turned on, and a refusal shows here with the fields
    /// still filled; it closes once the folder is on.
    @StateObject private var form: EnrollForm

    /// `form` is given only to show a state for a check; the app starts from an empty one.
    init(form: EnrollForm = EnrollForm()) {
        _form = StateObject(wrappedValue: form)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Turn on uploads for a folder")
                .font(.system(size: 15, weight: .semibold))
            Text("Every Claude Code and Codex conversation that runs in this folder, or below it, is saved after each turn and uploaded to the repository you choose.")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)

            VStack(alignment: .leading, spacing: 6) {
                Text("Folder").font(.system(size: 11, weight: .semibold))
                HStack {
                    Text(form.path.map(Words.tilde) ?? "No folder chosen")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(form.path == nil ? .secondary : .primary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer()
                    Button("Use Finder's window") { useFinder() }
                    Button("Choose…") { choose() }
                }
                // The form stays as it was while a try runs, so its answer is about what is shown.
                .disabled(form.running != nil)
                if let problem = form.finderProblem {
                    Text(problem).font(.system(size: 11)).foregroundStyle(.secondary)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Repository on \(Words.host(model.whoami?.hub ?? "the Hub"))")
                    .font(.system(size: 11, weight: .semibold))
                TextField("owner/name", text: $form.repository)
                    .textFieldStyle(.roundedBorder)
                    .font(.system(size: 12, design: .monospaced))
                Toggle("Create it as a private repository if it does not exist yet", isOn: $form.createPrivate)
                    .disabled(!ownRepository)
                Toggle("Also upload conversations that already ran in this folder", isOn: $form.includeHistory)
            }
            .font(.system(size: 12))
            .disabled(form.running != nil)

            if !model.signedIn {
                Text("Sign in first: uploads need your AgentGit account.")
                    .font(.system(size: 11))
                    .foregroundStyle(.orange)
            }
            if let refusal = form.refusal {
                VStack(alignment: .leading, spacing: 2) {
                    Text(refusal.title).font(.system(size: 11, weight: .semibold)).foregroundStyle(.orange)
                    ReasonText(text: refusal.reason)
                    if let move = refusal.move, let path = form.path {
                        Button("Move to \(move.to)…") {
                            form.refusal = nil
                            form.running = path
                            model.move(
                                path, move, includeHistory: form.includeHistory,
                                createPrivate: form.createPrivate && ownRepository
                            ) { on in
                                form.running = nil
                                if on == true { return dismiss() }
                                if on == false { refuse(path, move.to) }
                            }
                        }
                        .controlSize(.small)
                        .disabled(form.running != nil)
                    }
                }
            }

            HStack {
                if let running = form.running, let step = model.busy[running] {
                    ProgressView().controlSize(.small)
                    Text(step).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Turn on uploads…") {
                    guard let path = form.path else { return }
                    let repository = form.repository.trimmingCharacters(in: .whitespaces)
                    form.refusal = nil
                    form.running = path
                    model.enroll(
                        path: path, repository: repository,
                        includeHistory: form.includeHistory, createPrivate: form.createPrivate && ownRepository
                    ) { on in
                        form.running = nil
                        if on == true { return dismiss() }
                        // Cancelled in the question: back to the form as it was.
                        if on == false { refuse(path, repository) }
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(form.path == nil || !validRepository || !model.signedIn || form.running != nil)
            }
        }
        .padding(20)
        .frame(width: 480)
    }

    /// Shows the refusal of a try for `path` into `repository`, only while the form still shows
    /// those: a refusal, and the move it offers, belong to the folder and repository tried.
    private func refuse(_ path: String, _ repository: String) {
        guard form.path == path, form.repository.trimmingCharacters(in: .whitespaces) == repository else { return }
        form.refusal = model.failed[path]
    }

    private var parts: [String] {
        form.repository.trimmingCharacters(in: .whitespaces).split(separator: "/", omittingEmptySubsequences: false).map(String.init)
    }

    private var validRepository: Bool {
        parts.count == 2 && parts.allSatisfy { !$0.isEmpty && !$0.contains(" ") }
    }

    private var ownRepository: Bool {
        validRepository && parts[0] == model.whoami?.account
    }

    private func set(_ folder: String) {
        form.path = folder
        if let account = model.whoami?.account {
            form.repository = "\(account)/\(Self.slug((folder as NSString).lastPathComponent))"
        }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.prompt = "Choose"
        NSApp.activate(ignoringOtherApps: true)
        if panel.runModal() == .OK, let url = panel.url { set(url.path) }
    }

    /// Asks Finder for its front window's folder. macOS asks the user once before allowing it.
    private func useFinder() {
        let source = """
        tell application "Finder"
            if (count of Finder windows) is 0 then return ""
            return POSIX path of (target of front Finder window as alias)
        end tell
        """
        var error: NSDictionary?
        let result = NSAppleScript(source: source)?.executeAndReturnError(&error).stringValue
        if let folder = result, !folder.isEmpty {
            form.finderProblem = nil
            set(folder.hasSuffix("/") && folder.count > 1 ? String(folder.dropLast()) : folder)
        } else {
            form.finderProblem = error == nil
                ? "Finder has no open window."
                : "Finder did not answer. Allow AgentGit to control Finder in System Settings → Privacy & Security → Automation, or choose the folder."
        }
    }

    /// A repository name the Hub accepts, derived from the folder's name.
    static func slug(_ name: String) -> String {
        let allowed = Set("abcdefghijklmnopqrstuvwxyz0123456789-_.")
        let mapped = name.lowercased().map { allowed.contains($0) ? $0 : "-" }
        let collapsed = String(mapped).split(separator: "-", omittingEmptySubsequences: true).joined(separator: "-")
        return collapsed.isEmpty ? "sessions" : collapsed
    }
}
