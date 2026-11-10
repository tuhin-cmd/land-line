import Foundation
import LandlineKit
import Testing

@testable import Landline

func device(
    _ name: String,
    id: String? = nil,
    kind: DeviceKind = .phone,
    reachable: Bool = true,
    _ pairState: PairState = .notPaired,
    key: String? = nil,
    battery: Battery? = nil
) -> Device {
    Device(
        id: id ?? "id-\(name)", name: name, kind: kind, reachable: reachable,
        pairState: pairState, verificationKey: key, battery: battery)
}

@Suite struct RowContentTests {
    @Test(arguments: [
        (PairState.paired, true, RowState.connected, "Connected", [RowAction.ping, .unpair]),
        (.paired, false, .offline, "Offline", [.unpair]),
        (.notPaired, true, .available, "Available to pair", [.pair]),
        (.notPaired, false, .unavailable, "Not reachable", []),
        (.requested, true, .waitingForPeer, "Waiting for approval…", [.cancel]),
        (.requestedByPeer, true, .askingUs, "Wants to pair", [.reject, .accept]),
    ])
    func stateDecidesStatusAndActions(
        pairState: PairState, reachable: Bool, state: RowState, status: String, actions: [RowAction]
    ) {
        let content = DeviceRowContent(device("Phone", reachable: reachable, pairState))
        #expect(content.state == state)
        #expect(content.status == status)
        #expect(content.actions == actions)
    }

    @Test func pingShowsOnlyOnConnectedRows() {
        #expect(DeviceRowContent(device("A", .paired), pingReceived: true).status == "Ping received")
        #expect(DeviceRowContent(device("A", .notPaired), pingReceived: true).status == "Available to pair")
    }

    @Test func pendingKeyIsGrouped() {
        let content = DeviceRowContent(device("A", .requested, key: "3f9a0c7e"))
        #expect(content.keyGroups == ["3F9A", "0C7E"])
        #expect(DeviceRowContent(device("A", .paired)).keyGroups == nil)
    }

    @Test(arguments: [
        (DeviceKind.phone, "smartphone"), (.tablet, "ipad.landscape"), (.desktop, "desktopcomputer"),
        (.laptop, "laptopcomputer"), (.tv, "tv"),
    ])
    func everyKindHasASymbol(kind: DeviceKind, symbol: String) {
        #expect(DeviceRowContent(device("A", kind: kind)).symbol == symbol)
    }
}

@Suite struct BatteryTests {
    @Test(arguments: [
        (UInt8(100), false, "100%", "battery.100percent", false),
        (82, false, "82%", "battery.75percent", false),
        (50, false, "50%", "battery.50percent", false),
        (21, false, "21%", "battery.25percent", false),
        (20, false, "20%", "battery.25percent", true),
        (5, false, "5%", "battery.0percent", true),
        (5, true, "5%", "battery.100percent.bolt", false),
    ])
    func badge(charge: UInt8, charging: Bool, text: String, symbol: String, low: Bool) {
        let badge = BatteryBadge(Battery(charge: charge, charging: charging))
        #expect(badge.text == text)
        #expect(badge.symbol == symbol)
        #expect(badge.isLow == low)
    }

    @Test func spokenWithChargingState() {
        #expect(BatteryBadge(Battery(charge: 40, charging: true)).accessibilityLabel
            == "Battery 40 percent, charging")
    }

    @Test func outOfRangeChargeIsClamped() {
        #expect(BatteryBadge(Battery(charge: 250, charging: false)).text == "100%")
    }

    @Test func shownOnlyWhileConnected() {
        let battery = Battery(charge: 60, charging: false)
        #expect(DeviceRowContent(device("A", .paired, battery: battery)).battery?.text == "60%")
        #expect(DeviceRowContent(device("A", reachable: false, .paired, battery: battery)).battery == nil)
        #expect(DeviceRowContent(device("A", .paired)).battery == nil)
    }
}

@Suite struct VerificationKeyTests {
    @Test func splitsIntoTwoBlocksOfFour() {
        #expect(VerificationKey.groups("ABCD1234") == ["ABCD", "1234"])
    }

    @Test func uppercasesAndIgnoresSpaces() {
        #expect(VerificationKey.groups("ab cd 12 34") == ["ABCD", "1234"])
    }

    @Test func keepsAShortTail() {
        #expect(VerificationKey.groups("ABCDEF") == ["ABCD", "EF"])
        #expect(VerificationKey.groups("").isEmpty)
    }

    @Test func spokenFormSpellsCharacters() {
        #expect(VerificationKey.spokenForm("3F9A0C7E") == "3 F 9 A, 0 C 7 E")
    }
}

@Suite struct SectionTests {
    @Test func splitsByPairState() {
        let sections = DeviceSections([
            device("Asking", .requestedByPeer, key: "AAAABBBB"),
            device("Mine", .paired),
            device("Waiting", .requested, key: "CCCCDDDD"),
            device("New", .notPaired),
        ])
        #expect(sections.requests.map(\.name) == ["Asking"])
        #expect(sections.mine.map(\.name) == ["Mine"])
        #expect(sections.nearby.map(\.name) == ["New", "Waiting"])
    }

    @Test func connectedDevicesComeFirstThenByName() {
        let sections = DeviceSections([
            device("Zed", reachable: true, .paired),
            device("Alpha", reachable: false, .paired),
            device("Beta", reachable: true, .paired),
        ])
        #expect(sections.mine.map(\.name) == ["Beta", "Zed", "Alpha"])
    }

    @Test func namesSortLikeFinder() {
        let sections = DeviceSections([device("Pixel 10"), device("pixel 2"), device("Galaxy")])
        #expect(sections.nearby.map(\.name) == ["Galaxy", "pixel 2", "Pixel 10"])
    }

    @Test func sameNamesKeepAStableOrder() {
        let a = device("Phone", id: "a"), b = device("Phone", id: "b")
        #expect(DeviceSections([b, a]).nearby.map(\.id) == ["a", "b"])
    }

    @Test func emptyMeansNothingAnywhere() {
        #expect(DeviceSections([]).isEmpty)
        #expect(!DeviceSections([device("A", .requestedByPeer)]).isEmpty)
    }
}

@Suite struct NoticeTests {
    @Test(arguments: [PairFailure.timedOut, .cancelledByPeer, .clocksDoNotMatch, .notReachable])
    func everyFailureNamesTheDevice(reason: PairFailure) {
        let notice = Notice.pairingFailed(reason, deviceName: "Pixel 8a")
        #expect((notice.title + notice.message).contains("Pixel 8a"))
        #expect(!notice.message.isEmpty)
    }

    @Test func failuresAreWordedDifferently() {
        let reasons: [PairFailure] = [.timedOut, .cancelledByPeer, .clocksDoNotMatch, .notReachable]
        let titles = Set(reasons.map { Notice.pairingFailed($0, deviceName: "A").title })
        #expect(titles.count == reasons.count)
    }

    @Test func clockFailureSaysWhatToFix() {
        let notice = Notice.pairingFailed(.clocksDoNotMatch, deviceName: nil)
        #expect(notice.message.contains("30 minutes"))
        #expect(notice.message.contains("the phone"))
    }

    @Test func unknownDeviceStillReadsAsASentence() {
        #expect(Notice.pairingFailed(.cancelledByPeer, deviceName: nil).message.hasPrefix("The device "))
    }
}

@Suite struct MenuBarIconTests {
    @Test func reflectsTheMostUrgentState() {
        let request = device("A", .requestedByPeer)
        let connected = device("B", .paired)
        let offline = device("C", reachable: false, .paired)

        #expect(MenuBarIcon.symbol(devices: [], network: .ready, failedToStart: false) == "iphone.gen3")
        #expect(MenuBarIcon.symbol(devices: [offline], network: .ready, failedToStart: false) == "iphone.gen3")
        #expect(
            MenuBarIcon.symbol(devices: [connected], network: .ready, failedToStart: false)
                == "iphone.gen3.radiowaves.left.and.right")
        #expect(
            MenuBarIcon.symbol(devices: [connected, request], network: .ready, failedToStart: false)
                == "iphone.gen3.badge.exclamationmark")
        #expect(MenuBarIcon.symbol(devices: [request], network: .blocked, failedToStart: false) == "iphone.gen3.slash")
        #expect(MenuBarIcon.symbol(devices: [], network: nil, failedToStart: true) == "iphone.gen3.slash")
    }
}

@Suite struct LaunchModeTests {
    @Test func parsesDemoArguments() {
        #expect(LaunchMode(arguments: ["Landline"]) == .live)
        #expect(LaunchMode(arguments: ["Landline", "--demo"]) == .demo(.all))
        #expect(LaunchMode(arguments: ["Landline", "--demo=blocked"]) == .demo(.blocked))
        #expect(LaunchMode(arguments: ["Landline", "--demo=nonsense"]) == .demo(.all))
        // macOS adds its own arguments (such as -NSDocumentRevisionsDebugMode)
        // when launched from some tools; they must not switch modes.
        #expect(LaunchMode(arguments: ["Landline", "-NSRequiresAquaSystemAppearance", "YES"]) == .live)
    }
}

@Suite struct NotificationTimeTests {
    @Test func compactAge() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        func age(_ seconds: Double) -> String {
            NotificationTime.short(Int64((now.timeIntervalSince1970 - seconds) * 1000), now: now)
        }
        #expect(age(5) == "now")
        #expect(age(-30) == "now")  // phone clock slightly ahead
        #expect(age(5 * 60) == "5m")
        #expect(age(3 * 3600 + 59) == "3h")
        #expect(age(2 * 86_400) == "2d")
    }
}
