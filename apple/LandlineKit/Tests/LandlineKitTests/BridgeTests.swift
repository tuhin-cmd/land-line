import Foundation
import LandlineKit
import Testing

/// Proves the three FFI shapes the app depends on: a plain call, an async
/// call, and a callback from a core thread.
@Suite struct BridgeTests {
    @Test func versionIsReported() {
        #expect(coreVersion().split(separator: ".").count == 3)
    }

    @Test func asyncCallReturns() async throws {
        let core = try LandlineCore(delegate: Beats { _ in })
        #expect(try await core.echoAfter(text: "hello", delayMs: 10) == "hello")
    }

    @Test func delegateIsCalledBack() async throws {
        let (beats, continuation) = AsyncStream.makeStream(of: UInt64.self)
        let core = try LandlineCore(delegate: Beats { continuation.yield($0) })
        core.start()
        defer { core.stop() }

        var received: [UInt64] = []
        for await beat in beats.prefix(2) {
            received.append(beat)
        }
        #expect(received == [1, 2])
    }
}

private final class Beats: CoreDelegate {
    private let onBeat: @Sendable (UInt64) -> Void

    init(_ onBeat: @escaping @Sendable (UInt64) -> Void) {
        self.onBeat = onBeat
    }

    func heartbeat(count: UInt64) {
        onBeat(count)
    }
}
