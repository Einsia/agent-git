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
    @Published var path: String?
    @Published var repository = ""
    @Published var createPrivate = true
    @Published var includeHistory = false
    @Published var finderProblem: String?
}
