import SwiftUI

/// Home screen: the instance card, its hosts and the pairing entry point.
///
/// The layout follows the product rule that the app must be usable with one hand
/// and zero configuration: one big list of machines, one button to add another.
struct HomeView: View {
    @EnvironmentObject private var store: InstanceStore
    @EnvironmentObject private var controller: SessionController
    @State private var scanning = false

    var body: some View {
        NavigationStack {
            Group {
                if store.instances.isEmpty {
                    emptyState
                } else {
                    instanceList
                }
            }
            .navigationTitle("Waystone")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        scanning = true
                    } label: {
                        Label("Add device", systemImage: "plus")
                    }
                }
            }
            .sheet(isPresented: $scanning) {
                NavigationStack {
                    PairingScannerView { uri in
                        controller.pair(with: uri)
                    }
                }
            }
            .fullScreenCover(isPresented: streamingBinding) {
                StreamView()
                    .environmentObject(controller)
            }
            .alert(
                "Something went wrong",
                isPresented: errorBinding,
                actions: {
                    Button("OK", role: .cancel) {}
                },
                message: {
                    Text(controller.lastError ?? "Unknown error")
                }
            )
        }
    }

    private var emptyState: some View {
        VStack(spacing: 20) {
            Image(systemName: "rectangle.inset.filled.and.person.filled")
                .font(.system(size: 44))
                .foregroundStyle(.secondary)
            Text("No computers yet")
                .font(.title3.weight(.semibold))
            Text("Open the Waystone panel on your computer and scan the pairing code. No IP addresses, no ports.")
                .font(.footnote)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 32)
            Button {
                scanning = true
            } label: {
                Label("Scan pairing code", systemImage: "qrcode.viewfinder")
                    .padding(.horizontal, 8)
            }
            .buttonStyle(.borderedProminent)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var instanceList: some View {
        List {
            ForEach(store.instances) { instance in
                Section {
                    HStack {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(instance.label)
                                .font(.headline)
                            Text(instance.endpoint)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Spacer()
                        connectionBadge
                    }
                    ForEach(hostsFor(instance)) { host in
                        Button {
                            controller.connect(to: instance, andStart: host)
                        } label: {
                            HStack {
                                Image(systemName: icon(for: host.platform))
                                    .foregroundStyle(.secondary)
                                Text(host.name)
                                Spacer()
                                Circle()
                                    .fill(host.online ? Color.green : Color.secondary.opacity(0.4))
                                    .frame(width: 8, height: 8)
                            }
                        }
                        .disabled(!host.online)
                    }
                } header: {
                    Text(instance.label)
                } footer: {
                    if let lastSeen = instance.lastSeen {
                        Text("Last connected \(relative(lastSeen))")
                    }
                }
            }
            .onDelete { offsets in
                for index in offsets {
                    controller.forget(store.instances[index])
                }
            }
        }
    }

    @ViewBuilder
    private var connectionBadge: some View {
        switch controller.phase {
        case .pairing:
            ProgressView()
        case .online:
            Text("Online").font(.caption).foregroundStyle(.green)
        case .connecting:
            ProgressView()
        case .streaming:
            Text("Streaming").font(.caption).foregroundStyle(.green)
        case .ended(let reason):
            Text(reason).font(.caption).foregroundStyle(.orange).lineLimit(1)
        case .idle:
            EmptyView()
        }
    }

    private func hostsFor(_ instance: Instance) -> [Host] {
        // The pool is per instance on the server, so hosts are shown under the
        // instance that is currently connected.
        guard controller.phase == .online || controller.phase.isStreaming else { return [] }
        return controller.hosts
    }

    private func icon(for platform: String) -> String {
        switch platform {
        case "macos": return "macbook"
        case "windows": return "pc"
        case "linux": return "server.rack"
        default: return "desktopcomputer"
        }
    }

    private func relative(_ date: Date) -> String {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .short
        return formatter.localizedString(for: date, relativeTo: Date())
    }

    private var streamingBinding: Binding<Bool> {
        Binding(
            get: { controller.phase.isStreaming },
            set: { _ in }
        )
    }

    private var errorBinding: Binding<Bool> {
        Binding(
            get: { controller.lastError != nil && !controller.phase.isStreaming },
            set: { _ in controller.lastError = nil }
        )
    }
}
