import AppKit
import SwiftUI

/// A short tour of what agit does, each item with the command that does it.
struct GuideView: View {
    @EnvironmentObject private var model: AppModel

    private struct Feature: Identifiable {
        let title: String
        let summary: String
        let commands: [String]
        var id: String { title }
    }

    private let features = [
        Feature(
            title: "Save a folder's conversations automatically",
            summary: "Turn a project folder on once. Every Claude Code and Codex conversation in it is saved after each turn and uploaded to your repository on the Hub.",
            commands: [
                "agit project bind <folder> --repo <owner/name> --history none --auto-upload",
                "agit project list",
            ]),
        Feature(
            title: "Continue any saved conversation",
            summary: "Each turn you send is one commit. Pick a conversation up again later, in the same agent or another one.",
            commands: ["agit resume", "agit log"]),
        Feature(
            title: "Share one conversation",
            summary: "Create a read-only link to a conversation. Links expire on their own, and you can revoke them.",
            commands: ["agit share", "agit share list"]),
        Feature(
            title: "Search everything you saved",
            summary: "Find sessions, repositories, pull requests and people on the Hub.",
            commands: ["agit search \"<words>\""]),
        Feature(
            title: "Fork and merge conversations",
            summary: "Start a new conversation from any earlier turn, and let a merge agent combine two of them.",
            commands: ["agit fork", "agit merge"]),
        Feature(
            title: "Control this Mac's agents remotely",
            summary: "Drive Claude Code and Codex on this Mac from the Hub.",
            commands: ["agit rc"]),
        Feature(
            title: "Keep secrets out of uploads",
            summary: "Every upload is checked for credentials first. Register your own secrets so they never leave this Mac.",
            commands: ["agit scan", "agit secrets"]),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("What agit can do")
                .font(.system(size: 15, weight: .semibold))
                .padding([.horizontal, .top], 20)
                .padding(.bottom, 4)
            Text("Everything here also works from a terminal. Copy a command to try it.")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 20)
                .padding(.bottom, 12)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    ForEach(features) { feature in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(feature.title).font(.system(size: 13, weight: .semibold))
                            Text(feature.summary)
                                .font(.system(size: 12))
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                            ForEach(feature.commands, id: \.self) { CommandLine(command: $0) }
                        }
                    }
                }
                .padding(20)
            }
            Divider()
            HStack {
                if let hub = model.whoami?.hub, let account = model.whoami?.account {
                    Button("Open your page on the Hub") { model.open("\(hub)/@\(account)") }
                }
                Spacer()
                Button("Turn on a folder…") { model.notice = nil; openEnroll() }
            }
            .buttonStyle(.borderless)
            .font(.system(size: 12))
            .padding(14)
        }
        .frame(width: 520, height: 560)
    }

    @Environment(\.openWindow) private var openWindow

    private func openEnroll() {
        openWindow(id: "enroll")
        NSApp.activate(ignoringOtherApps: true)
    }
}

private struct CommandLine: View {
    @EnvironmentObject private var model: AppModel
    let command: String

    var body: some View {
        HStack {
            Text(command)
                .font(.system(size: 11, design: .monospaced))
                .textSelection(.enabled)
                .lineLimit(2)
            Spacer()
            Button { model.copy(command, what: "the command") } label: { Image(systemName: "doc.on.doc") }
                .buttonStyle(.borderless)
                .help("Copy")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .background(RoundedRectangle(cornerRadius: 5).fill(Color.secondary.opacity(0.08)))
    }
}
