import CoreGraphics
import Foundation

/// View-local state. Views keep it in an `@StateObject` rather than `@State`: the Command Line
/// Tools SDK declares `State` as a macro without shipping the plugin that expands it, so a view
/// with `@State` builds only with Xcode.
final class Flag: ObservableObject {
    @Published var on: Bool

    init(_ on: Bool) { self.on = on }
}

final class Measured: ObservableObject {
    @Published var value: CGFloat = 0
}

final class EnrollForm: ObservableObject {
    /// A refusal, and the move it offers, are about the folder and repository tried; changing
    /// either drops them, so a move is never offered for a folder it was not refused for.
    @Published var path: String? { didSet { if path != oldValue { refusal = nil } } }
    @Published var repository = "" { didSet { if repository != oldValue { refusal = nil } } }
    @Published var createPrivate = true
    @Published var includeHistory = false
    @Published var finderProblem: String?
    /// The folder being turned on, while it runs.
    @Published var running: String?
    /// Why the last try did not turn the folder on.
    @Published var refusal: Failure?
}
