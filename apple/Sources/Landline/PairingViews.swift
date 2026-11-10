import LandlineKit
import SwiftUI

/// The verification key in two blocks of four, sized to be compared with the
/// phone's screen at arm's length.
struct VerificationKeyView: View {
    let key: String

    var body: some View {
        HStack(spacing: 8) {
            ForEach(Array(VerificationKey.groups(key).enumerated()), id: \.offset) { _, group in
                Text(group)
                    .font(.system(size: 20, weight: .semibold, design: .monospaced))
                    .tracking(3)
                    .padding(.leading, 3) // tracking adds space after the last letter only
                    .padding(.horizontal, 10)
                    .padding(.vertical, 5)
                    .background(.background, in: RoundedRectangle(cornerRadius: 6, style: .continuous))
            }
        }
        .frame(maxWidth: .infinity)
        .textSelection(.disabled)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Verification key \(VerificationKey.spokenForm(key))")
    }
}

/// A tinted container shared by the pairing cards and banners, the way the
/// system panels group a related block of controls.
struct InsetCard<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) { content }
            .padding(10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.quinary, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

/// Shown under a nearby device's row after we asked it to pair.
struct OutgoingPairingCard: View {
    let device: Device
    let onCancel: () -> Void

    var body: some View {
        InsetCard {
            Text("On \(device.name), accept the request if it shows this key:")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let key = device.verificationKey {
                VerificationKeyView(key: key)
            }
            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                    .controlSize(.small)
            }
        }
    }
}

/// A phone asking to pair. Pulled to the top of the popover because the
/// request expires after about 25 seconds.
struct IncomingPairingCard: View {
    let device: Device
    let onReject: () -> Void
    let onAccept: () -> Void

    var body: some View {
        InsetCard {
            HStack(spacing: 8) {
                DeviceIcon(symbol: device.kind.symbol, highlighted: true)
                VStack(alignment: .leading, spacing: 1) {
                    Text("\(device.name) wants to pair")
                        .font(.headline)
                        .lineLimit(1)
                    Text("Accept only if the phone shows the same key.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            if let key = device.verificationKey {
                VerificationKeyView(key: key)
            }
            HStack {
                Spacer()
                Button("Reject", action: onReject)
                // Not bound to Return: accepting should take a deliberate
                // click after comparing the key, never a stray keypress.
                Button("Accept", action: onAccept)
                    .buttonStyle(.borderedProminent)
            }
            .controlSize(.small)
        }
    }
}
