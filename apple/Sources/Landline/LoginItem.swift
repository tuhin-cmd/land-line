import Foundation
import ServiceManagement

enum LoginItemState: Equatable, Sendable {
    case enabled
    case disabled
    /// Registered, but the user has to allow it in System Settings.
    case needsApproval
    /// macOS does not know this copy of the app as something that can open
    /// at login.
    case unavailable
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
        case .notRegistered: .disabled
        case .requiresApproval: .needsApproval
        case .notFound: .unavailable
        @unknown default: .unavailable
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
        case .unavailable:
            "macOS can’t open this copy of Landline at login. Move Landline to the Applications folder and open it from there."
        }
    }

    /// What to say when turning it on or off failed. Development builds are
    /// signed ad hoc and run from the build folder, which macOS often refuses.
    static func failureMessage(enabling: Bool, error: any Error) -> String {
        let action = enabling ? "turn on" : "turn off"
        return "macOS didn’t let Landline \(action) opening at login (\(error.localizedDescription)). This is expected for a development build run from the build folder; a signed copy in Applications can do it."
    }
}
