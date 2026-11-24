import SwiftUI

/// Home: every machine in every enrolled pool, one tap to stream.
///
/// Design doc 3.2: from opening the app to seeing the home computer must take
/// under 30 seconds and no typing, so the only actions here are "tap a
/// computer" and "add a server".
struct HomeView: View {
    @EnvironmentObject private var model: AppModel
    @State private var addingServer = false

    var body: some View {
        NavigationStack {
            Group {
                if model.pools.isEmpty {
                    EmptyPoolView { addingServer = true }
                } else {
                    poolList
                }
            }
            .navigationTitle("Waystone")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        addingServer = true
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("添加服务器")
                }
            }
        }
        .sheet(isPresented: $addingServer) {
            PairingSheet()
        }
        .overlay(alignment: .bottom) { noticeOverlay }
        .alert("添加失败", isPresented: pairingFailed) {
            Button("好", role: .cancel) { model.dismissPairingError() }
        } message: {
            if case .failed(let reason) = model.pairing { Text(reason) }
        }
    }

    private var poolList: some View {
        List {
            ForEach(model.pools) { pool in
                PoolSection(pool: pool)
            }
        }
        .listStyle(.insetGrouped)
        .refreshable { model.refresh() }
    }

    @ViewBuilder
    private var noticeOverlay: some View {
        VStack(spacing: 8) {
            if case .inProgress(let endpoint) = model.pairing {
                NoticeCard(symbol: nil, title: "正在添加 \(endpoint)…", message: nil)
            }
            if let notice = model.notice {
                NoticeCard(symbol: "info.circle", title: notice.title, message: notice.message)
                    .onTapGesture { model.notice = nil }
                    .task(id: notice.id) {
                        try? await Task.sleep(for: .seconds(6))
                        if model.notice?.id == notice.id { model.notice = nil }
                    }
            }
        }
        .padding()
        .animation(.spring(duration: 0.3), value: model.notice)
        .animation(.spring(duration: 0.3), value: model.pairing)
    }

    private var pairingFailed: Binding<Bool> {
        Binding(
            get: { if case .failed = model.pairing { return true } else { return false } },
            set: { if !$0 { model.dismissPairingError() } })
    }
}

/// One server and its machines.
private struct PoolSection: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var pool: PoolConnection
    @State private var confirmForget = false

    var body: some View {
        Section {
            switch pool.status {
            case .online where pool.hosts.isEmpty:
                Label("还没有电脑加入。在电脑上运行 `waystone-host pair` 并粘贴面板上的主机配对码。", systemImage: "desktopcomputer")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            case .rejected(let reason):
                Label(reason, systemImage: "exclamationmark.octagon")
                    .font(.footnote)
                    .foregroundStyle(.red)
            default:
                ForEach(pool.hosts) { host in
                    HostRow(host: host, reachable: pool.isOnline) {
                        model.connect(to: host, in: pool)
                    }
                }
                if pool.hosts.isEmpty, case .connecting = pool.status {
                    HStack {
                        ProgressView()
                        Text("正在连接服务器…").foregroundStyle(.secondary)
                    }
                }
            }
        } header: {
            HStack {
                Text(pool.instance.label)
                Spacer()
                StatusPill(status: pool.status) { pool.reconnectNow() }
                Menu {
                    Button("重新连接", systemImage: "arrow.clockwise") { pool.reconnectNow() }
                    Button("移除此服务器", systemImage: "trash", role: .destructive) { confirmForget = true }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .accessibilityLabel("服务器选项")
            }
        }
        .confirmationDialog(
            "移除 \(pool.instance.label)？", isPresented: $confirmForget, titleVisibility: .visible
        ) {
            Button("移除", role: .destructive) { model.forget(pool) }
        } message: {
            Text("之后需要重新扫码才能连接这台服务器上的电脑。")
        }
    }
}

private struct StatusPill: View {
    var status: PoolConnection.Status
    var retry: () -> Void

    var body: some View {
        switch status {
        case .online:
            Label("在线", systemImage: "circle.fill")
                .labelStyle(DotLabelStyle(color: .green))
        case .connecting:
            ProgressView().controlSize(.mini)
        case .offline(let reason):
            Button(action: retry) {
                Label(reason, systemImage: "circle.fill")
                    .labelStyle(DotLabelStyle(color: .orange))
            }
            .buttonStyle(.plain)
        case .rejected:
            Label("已移除", systemImage: "circle.fill")
                .labelStyle(DotLabelStyle(color: .red))
        }
    }
}

private struct DotLabelStyle: LabelStyle {
    var color: Color

    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 4) {
            Circle().fill(color).frame(width: 6, height: 6)
            configuration.title
                .font(.caption2)
                .lineLimit(1)
                .textCase(nil)
        }
    }
}

/// A machine card: big, one tap, with an honest state.
private struct HostRow: View {
    var host: Host
    var reachable: Bool
    var connect: () -> Void

    private var available: Bool { reachable && host.online && !host.busy }

    var body: some View {
        Button(action: connect) {
            HStack(spacing: 14) {
                Image(systemName: symbol)
                    .font(.title2)
                    .frame(width: 36)
                    .foregroundStyle(available ? Color.accentColor : .secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(host.name)
                        .font(.body.weight(.medium))
                        .foregroundStyle(available ? .primary : .secondary)
                    Text(subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if available {
                    Image(systemName: "play.circle.fill")
                        .font(.title2)
                        .foregroundStyle(Color.accentColor)
                }
            }
            .padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!available)
        .accessibilityHint(available ? "开始串流" : subtitle)
    }

    private var subtitle: String {
        guard reachable else { return "服务器未连接" }
        guard host.online else { return "离线或休眠" }
        if host.busy { return "正在被其他设备使用" }
        if let rtt = host.rttMs { return "在线 · 服务器延迟 \(rtt) ms" }
        return "在线"
    }

    private var symbol: String {
        switch host.platform {
        case .macos: return "laptopcomputer"
        case .windows: return "pc"
        case .linux: return "server.rack"
        default: return "desktopcomputer"
        }
    }
}

private struct EmptyPoolView: View {
    var add: () -> Void

    var body: some View {
        ContentUnavailableView {
            Label("还没有电脑", systemImage: "desktopcomputer")
        } description: {
            Text("打开 Waystone 服务器面板，点「添加我的设备」，然后扫描二维码。\n不用输入 IP、端口或密码。")
        } actions: {
            Button(action: add) {
                Label("扫码添加", systemImage: "qrcode.viewfinder")
                    .padding(.horizontal, 8)
            }
            .buttonStyle(.borderedProminent)
        }
    }
}

struct NoticeCard: View {
    var symbol: String?
    var title: String
    var message: String?

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            if let symbol {
                Image(systemName: symbol).foregroundStyle(.orange)
            } else {
                ProgressView()
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.subheadline.weight(.semibold))
                if let message {
                    Text(message).font(.footnote).foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(14)
        .frame(maxWidth: 480)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
        .transition(.move(edge: .bottom).combined(with: .opacity))
    }
}
