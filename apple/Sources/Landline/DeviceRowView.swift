import LandlineKit
import SwiftUI

/// The round device glyph used by the system's Bluetooth panel: filled with
/// the accent colour when connected, grey otherwise.
struct DeviceIcon: View {
    let symbol: String
    let highlighted: Bool

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: 13))
            .foregroundStyle(highlighted ? AnyShapeStyle(.white) : AnyShapeStyle(.primary))
            .frame(width: 26, height: 26)
            .background(
                highlighted ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.quaternary),
                in: Circle())
            .accessibilityHidden(true)
    }
}

struct DeviceRowView: View {
    let device: Device
    @Environment(AppModel.self) private var model
    @State private var hovering = false

    private var content: DeviceRowContent {
        DeviceRowContent(device, pingReceived: model.recentPings.contains(device.id))
    }

    var body: some View {
        let content = content
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                DeviceIcon(symbol: content.symbol, highlighted: content.state == .connected)
                    .opacity(content.state == .offline ? 0.6 : 1)
                VStack(alignment: .leading, spacing: 1) {
                    Text(content.name)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    status(content)
                }
                // Only the text is merged. Merging the whole row would fold
                // its buttons into one element, and pressing that element
                // can fire a button in another row, such as Accept.
                .accessibilityElement(children: .combine)
                Spacer(minLength: 8)
                trailingControls(content)
            }
            .padding(.vertical, 4)
            .padding(.horizontal, 6)
            .background(
                hovering ? AnyShapeStyle(.quinary) : AnyShapeStyle(.clear),
                in: RoundedRectangle(cornerRadius: 6, style: .continuous))
            .onHover { hovering = $0 }

            if content.state == .waitingForPeer {
                OutgoingPairingCard(device: device) { model.perform(.cancel, on: device.id) }
            }
            if model.confirmingUnpair == device.id {
                unpairConfirmation
            }
        }
    }

    @ViewBuilder
    private func status(_ content: DeviceRowContent) -> some View {
        let pinged = model.recentPings.contains(device.id) && content.state == .connected
        HStack(spacing: 3) {
            if pinged {
                Image(systemName: "hand.wave.fill")
                    .foregroundStyle(Color.accentColor)
            }
            Text(content.status)
        }
        .font(.subheadline)
        .foregroundStyle(.secondary)
        .animation(.default, value: pinged)
    }

    @ViewBuilder
    private func trailingControls(_ content: DeviceRowContent) -> some View {
        HStack(spacing: 4) {
            if content.actions.contains(.pair) {
                Button("Pair") { model.perform(.pair, on: device.id) }
                    .controlSize(.small)
            }
            if content.state == .waitingForPeer {
                ProgressView()
                    .controlSize(.small)
            }
            if content.actions.contains(.ping) {
                let sent = model.sentPings.contains(device.id)
                Button {
                    model.perform(.ping, on: device.id)
                } label: {
                    Image(systemName: sent ? "checkmark" : "hand.wave")
                        .frame(width: 18, height: 18)
                        .contentTransition(.symbolEffect(.replace))
                }
                .buttonStyle(.borderless)
                .help("Ping \(device.name)")
                .accessibilityLabel("Ping \(device.name)")
            }
            if content.actions.contains(.unpair) {
                Menu {
                    Button("Unpair…") { model.perform(.unpair, on: device.id) }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("More")
                .accessibilityLabel("More actions for \(device.name)")
            }
        }
        .foregroundStyle(.secondary)
    }

    /// Inline rather than an alert: an alert on a menu bar panel can close
    /// the panel or open behind other windows.
    private var unpairConfirmation: some View {
        InsetCard {
            Text("Unpair \(device.name)?")
                .font(.headline)
            Text("It will need to be paired again, with a new key, before it can connect.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Spacer()
                Button("Cancel") { model.confirmingUnpair = nil }
                    .keyboardShortcut(.cancelAction)
                Button("Unpair", role: .destructive) { model.confirmUnpair() }
            }
            .controlSize(.small)
        }
    }
}
