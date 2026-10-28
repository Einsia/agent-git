import Foundation

/// One `agit --json` result. The app reads agit's state only through this envelope; it never
/// parses the human output of a command.
struct CLIResult {
    let ok: Bool
    let exitCode: Int
    /// `result.kind`; `parse_error` means this agit does not know the command.
    let kind: String?
    /// `result.value` as raw JSON, present when the command emitted one JSON document.
    let value: Data?
    /// stderr diagnostics with level `error`, in order.
    let errors: [String]

    /// The first error, or a generic sentence when agit gave none.
    var reason: String {
        errors.first ?? "agit stopped with exit code \(exitCode)"
    }

    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        guard let value else { throw CLIError.noValue }
        return try JSONDecoder.agit.decode(T.self, from: value)
    }
}

enum CLIError: LocalizedError {
    case notFound
    case noValue
    case unreadable(String)
    case timedOut

    var errorDescription: String? {
        switch self {
        case .notFound: return "agit is not installed, or it is not where this app looks for it."
        case .noValue: return "agit answered without the expected data."
        case .unreadable(let detail): return "agit's answer could not be read (\(detail))."
        case .timedOut: return "agit did not answer in time."
        }
    }
}

extension JSONDecoder {
    static let agit: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return decoder
    }()
}

/// Runs the installed agit. Every call adds `--json`, closes stdin and passes no credentials:
/// agit reads its own credential store.
struct AgitCLI {
    /// Defaults key for an explicit binary, set from the app or with
    /// `defaults write com.agentgit.menubar agitPath /path/to/agit`.
    static let pathKey = "agitPath"

    let executable: URL

    /// The binary the user chose, then `AGIT_MENUBAR_CLI`, then the usual install locations, then
    /// the login shell's PATH: an app started from Finder does not inherit a terminal's PATH.
    static func locate() -> AgitCLI? {
        let fileManager = FileManager.default
        var candidates: [String] = []
        if let chosen = UserDefaults.standard.string(forKey: pathKey) { candidates.append(chosen) }
        if let env = ProcessInfo.processInfo.environment["AGIT_MENUBAR_CLI"] { candidates.append(env) }
        let home = fileManager.homeDirectoryForCurrentUser.path
        candidates += [
            "\(home)/.local/bin/agit",
            "/usr/local/bin/agit",
            "/opt/homebrew/bin/agit",
            "\(home)/.npm-global/bin/agit",
        ]
        for path in candidates where fileManager.isExecutableFile(atPath: path) {
            return AgitCLI(executable: URL(fileURLWithPath: path))
        }
        if let path = loginShellLookup(), fileManager.isExecutableFile(atPath: path) {
            return AgitCLI(executable: URL(fileURLWithPath: path))
        }
        return nil
    }

    private static func loginShellLookup() -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/zsh")
        process.arguments = ["-lc", "command -v agit"]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        process.standardInput = FileHandle.nullDevice
        do { try process.run() } catch { return nil }
        process.waitUntilExit()
        let output = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return output?.isEmpty == false ? output : nil
    }

    /// The resolved program and when it was last written, read without running it: it changes
    /// when the program is replaced or another one is chosen.
    var stamp: String {
        let resolved = executable.resolvingSymlinksInPath()
        let written = (try? FileManager.default.attributesOfItem(atPath: resolved.path))?[.modificationDate] as? Date
        return "\(resolved.path)@\(written?.timeIntervalSince1970 ?? 0)"
    }

    /// The one line `agit --version` prints, shown to the user as is.
    func version() async -> String? {
        guard let (data, _) = try? await launch(["--version"], timeout: 10) else { return nil }
        return String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Runs `agit --json <arguments>` and decodes the envelope. A failed command is a result with
    /// `ok == false`, not a thrown error; only a missing or unreadable answer throws.
    func run(_ arguments: [String], timeout: TimeInterval = 60) async throws -> CLIResult {
        let (data, status) = try await launch(["--json", "--no-color"] + arguments, timeout: timeout)
        guard let document = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw CLIError.unreadable("exit code \(status)")
        }
        let result = document["result"] as? [String: Any]
        let value = result?["value"].flatMap {
            try? JSONSerialization.data(withJSONObject: $0, options: [.fragmentsAllowed])
        }
        let diagnostics = (document["diagnostics"] as? [String: Any])?["stderr"] as? [[String: Any]] ?? []
        let errors = diagnostics
            .filter { ($0["level"] as? String) == "error" }
            .compactMap { $0["message"] as? String }
        return CLIResult(
            ok: document["ok"] as? Bool ?? false,
            exitCode: document["exit_code"] as? Int ?? Int(status),
            kind: result?["kind"] as? String,
            value: value,
            errors: errors
        )
    }

    private func launch(_ arguments: [String], timeout: TimeInterval) async throws -> (Data, Int32) {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        var environment = ProcessInfo.processInfo.environment
        // A session variable inherited from a terminal would make agit act on that session.
        environment.removeValue(forKey: "AGIT_SESSION")
        environment["AGIT_TUI"] = "0"
        process.environment = environment
        process.currentDirectoryURL = FileManager.default.homeDirectoryForCurrentUser
        process.standardInput = FileHandle.nullDevice
        let output = Pipe()
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice

        return try await withCheckedThrowingContinuation { continuation in
            let collected = OutputBuffer()
            output.fileHandleForReading.readabilityHandler = { handle in
                collected.append(handle.availableData)
            }
            let timer = DispatchSource.makeTimerSource(queue: .global())
            timer.schedule(deadline: .now() + timeout)
            timer.setEventHandler {
                collected.timedOut = true
                process.terminate()
            }
            process.terminationHandler = { process in
                timer.cancel()
                output.fileHandleForReading.readabilityHandler = nil
                collected.append(output.fileHandleForReading.readDataToEndOfFile())
                if collected.timedOut {
                    continuation.resume(throwing: CLIError.timedOut)
                } else {
                    continuation.resume(returning: (collected.data, process.terminationStatus))
                }
            }
            do {
                try process.run()
                timer.resume()
            } catch {
                output.fileHandleForReading.readabilityHandler = nil
                continuation.resume(throwing: CLIError.notFound)
            }
        }
    }
}

/// stdout collected from a reader thread while the process runs, so a large answer never fills
/// the pipe and stalls agit.
private final class OutputBuffer: @unchecked Sendable {
    private let lock = NSLock()
    private var buffer = Data()
    private var expired = false

    func append(_ chunk: Data) {
        lock.lock()
        buffer.append(chunk)
        lock.unlock()
    }

    var data: Data {
        lock.lock()
        defer { lock.unlock() }
        return buffer
    }

    var timedOut: Bool {
        get {
            lock.lock()
            defer { lock.unlock() }
            return expired
        }
        set {
            lock.lock()
            expired = newValue
            lock.unlock()
        }
    }
}
