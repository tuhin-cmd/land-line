import Foundation
import ServiceManagement

enum LoginItemState: Equatable, Sendable {
    case enabled
    case disabled
    /// Registered, but the user has to allow it in System Settings.
    case needsApproval
}

@MainActor
protocol LoginItem: AnyObject {
    var state: LoginItemState { get }
    func setEnabled(_ enabled: Bool) throws
}

/// The app itself as a login item, through `SMAppService`.
@MainActor
final class SystemLoginItem: LoginItem {
    var state: LoginItemState {
        switch SMAppService.mainApp.status {
        case .enabled: .enabled
        case .requiresApproval: .needsApproval
        // `notFound` is also what an app that was never registered reports
        // (seen with an ad-hoc signed build in the build folder), so it
        // reads as off; a real failure shows when registering throws.
        case .notRegistered, .notFound: .disabled
        @unknown default: .disabled
        }
    }

    func setEnabled(_ enabled: Bool) throws {
        if enabled {
            try SMAppService.mainApp.register()
        } else {
            try SMAppService.mainApp.unregister()
        }
    }

    static func openLoginItemsSettings() {
        SMAppService.openSystemSettingsLoginItems()
    }
}

/// Remembers the choice without telling macOS: for demo mode and tests.
@MainActor
final class MemoryLoginItem: LoginItem {
    var state: LoginItemState
    var failure: (any Error)?

    init(state: LoginItemState = .disabled) {
        self.state = state
    }

    func setEnabled(_ enabled: Bool) throws {
        if let failure { throw failure }
        state = enabled ? .enabled : .disabled
    }
}

extension LoginItemState {
    /// The line under the toggle, if the state needs one.
    var explanation: String? {
        switch self {
        case .enabled, .disabled: nil
        case .needsApproval:
            "Waiting for your approval. Turn on Landline in System Settings > General > Login Items."
        }
    }

    /// What to say when turning it on or off failed. Development builds are
    /// signed ad hoc and run from the build folder, which macOS may refuse.
    static func failureMessage(enabling: Bool, error: (any Error)?) -> String {
        let action = enabling ? "turn on" : "turn off"
        let reason = error.map { " (\($0.localizedDescription))" } ?? ""
        return "macOS didn’t let Landline \(action) opening at login\(reason). Development builds run from the build folder can be refused; a signed copy in Applications should work."
    }
}
