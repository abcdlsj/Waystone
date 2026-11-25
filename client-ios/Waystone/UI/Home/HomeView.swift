import SwiftUI

/// Home: every machine in every enrolled pool, one tap to stream.
///
/// Design doc 3.2: from opening the app to seeing the home computer must take
/// under 30 seconds and no typing, so the only actions here are "tap a
/// computer" and "add a server".
struct HomeView: View {
    @EnvironmentObject private var model: AppModel
    @State private var addingServer = false
    @State private var showingSettings = false

    var body: some View {
        VStack(spacing: 0) {
            topBar

            if model.pools.isEmpty {
                EmptyPoolView {
                    Haptics.tap()
                    addingServer = true
                }
            } else {
                poolList
            }
        }
        .background(WaystoneTheme.canvas.ignoresSafeArea())
        .sheet(isPresented: $addingServer) {
            PairingSheet()
                .presentationDragIndicator(.visible)
        }
        .sheet(isPresented: $showingSettings) {
            SettingsSheet(preferences: model.preferences)
                .presentationDetents([.medium])
                .presentationDragIndicator(.visible)
        }
        .overlay(alignment: .bottom) { noticeOverlay }
        .alert("添加失败", isPresented: pairingFailed) {
            Button("好", role: .cancel) { model.dismissPairingError() }
        } message: {
            if case .failed(let reason) = model.pairing { Text(reason) }
        }
    }

    private var topBar: some View {
        VStack(spacing: 0) {
            HStack {
                HStack(spacing: 8) {
                    WaystoneMark()
                        .fill(WaystoneTheme.textPrimary, style: FillStyle(eoFill: true))
                        .frame(width: 15, height: 15)
                    Text("WAYSTONE")
                        .font(.system(size: 13, weight: .bold, design: .monospaced))
                        .tracking(1.8)
                        .foregroundStyle(WaystoneTheme.textPrimary)
                }

                Spacer()

                HStack(spacing: 8) {
                    Button {
                        Haptics.tap()
                        showingSettings = true
                    } label: {
                        Image(systemName: "gearshape")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(WaystoneTheme.textSecondary)
                            .frame(width: 28, height: 28)
                            .background(
                                RoundedRectangle(cornerRadius: 5, style: .continuous)
                                    .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                            )
                    }
                    .buttonStyle(ConsoleButtonStyle())
                    .accessibilityLabel("设置")

                    Button {
                        Haptics.tap()
                        addingServer = true
                    } label: {
                        HStack(spacing: 4) {
                            Image(systemName: "plus")
                                .font(.system(size: 11, weight: .semibold))
                            Text("添加")
                                .font(.system(size: 12, weight: .medium))
                        }
                        .foregroundStyle(WaystoneTheme.textPrimary)
                        .padding(.horizontal, 10)
                        .frame(height: 28)
                        .background(
                            RoundedRectangle(cornerRadius: 5, style: .continuous)
                                .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                        )
                    }
                    .buttonStyle(ConsoleButtonStyle())
                    .accessibilityLabel("添加服务器")
                }
            }
            .padding(.horizontal, 18)
            .frame(height: 48)

            Rectangle()
                .fill(WaystoneTheme.borderSubtle)
                .frame(height: 0.75)
        }
    }

    private var poolList: some View {
        ScrollView {
            LazyVStack(spacing: 24) {
                ForEach(model.pools) { pool in
                    PoolSection(pool: pool)
                }
            }
            .padding(.top, 16)
            .padding(.bottom, 36)
        }
        .scrollContentBackground(.hidden)
        .refreshable {
            Haptics.tap()
            model.refresh()
        }
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
        .padding(.horizontal, 16)
        .padding(.bottom, 16)
        .animation(.easeOut(duration: 0.2), value: model.notice)
        .animation(.easeOut(duration: 0.2), value: model.pairing)
    }

    private var pairingFailed: Binding<Bool> {
        Binding(
            get: { if case .failed = model.pairing { return true } else { return false } },
            set: { if !$0 { model.dismissPairingError() } })
    }
}

/// One server and its machines rendered as a full-bleed hairline console ledger.
private struct PoolSection: View {
    @EnvironmentObject private var model: AppModel
    @ObservedObject var pool: PoolConnection
    @State private var confirmForget = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionHeader

            VStack(spacing: 0) {
                hairline

                switch pool.status {
                case .online where pool.hosts.isEmpty:
                    emptyHostsRow
                case .rejected(let reason):
                    rejectedRow(reason: reason)
                default:
                    if pool.hosts.isEmpty, case .connecting = pool.status {
                        connectingRow
                    } else if pool.hosts.isEmpty {
                        offlineRow
                    } else {
                        ForEach(Array(pool.hosts.enumerated()), id: \.element.id) { index, host in
                            if index > 0 {
                                hairline
                            }
                            HostRow(host: host, reachable: pool.isOnline) {
                                Haptics.tap()
                                model.connect(to: host, in: pool)
                            }
                        }
                    }
                }

                hairline
            }
            .background(WaystoneTheme.ledger)
        }
        .confirmationDialog(
            "移除 \(pool.instance.label)？", isPresented: $confirmForget, titleVisibility: .visible
        ) {
            Button("移除", role: .destructive) { model.forget(pool) }
        } message: {
            Text("移除后需重新扫码添加。")
        }
    }

    private var sectionHeader: some View {
        HStack(spacing: 10) {
            Text(pool.instance.endpoint)
                .font(.system(size: 11.5, weight: .semibold, design: .monospaced))
                .foregroundStyle(WaystoneTheme.textSecondary)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: 8)

            PoolStatusLabel(status: pool.status) {
                Haptics.tap()
                pool.reconnectNow()
            }

            Menu {
                Button("重新连接", systemImage: "arrow.clockwise") { pool.reconnectNow() }
                Button("移除此服务器", systemImage: "trash", role: .destructive) { confirmForget = true }
            } label: {
                Image(systemName: "ellipsis")
                    .font(.system(size: 13, weight: .medium))
                    .foregroundStyle(WaystoneTheme.textTertiary)
                    .frame(width: 24, height: 22)
                    .contentShape(Rectangle())
            }
            .accessibilityLabel("服务器选项")
        }
        .padding(.horizontal, 18)
    }

    private var emptyHostsRow: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("暂无主机")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(WaystoneTheme.textPrimary)
            Text("在电脑上运行以下命令并粘贴主机配对码：")
                .font(.system(size: 12.5))
                .foregroundStyle(WaystoneTheme.textSecondary)
                .fixedSize(horizontal: false, vertical: true)
            Text("waystone-host pair")
                .font(.system(size: 12, weight: .medium, design: .monospaced))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
                .background(
                    RoundedRectangle(cornerRadius: 4, style: .continuous)
                        .fill(Color.black)
                        .overlay(
                            RoundedRectangle(cornerRadius: 4, style: .continuous)
                                .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                        )
                )
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
    }

    private func rejectedRow(reason: String) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "exclamationmark.octagon")
                .font(.system(size: 14, weight: .medium))
                .foregroundStyle(WaystoneTheme.coral)
            Text(reason)
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
    }

    private var connectingRow: some View {
        HStack(spacing: 10) {
            ProgressView()
                .controlSize(.small)
                .tint(WaystoneTheme.textSecondary)
            Text("连接中…")
                .font(.system(size: 13, weight: .regular))
                .foregroundStyle(WaystoneTheme.textSecondary)
            Spacer()
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
    }

    private var offlineRow: some View {
        HStack(spacing: 12) {
            Text("未连接")
                .font(.system(size: 13, weight: .regular))
                .foregroundStyle(WaystoneTheme.textSecondary)
            Spacer()
            Button("重试") {
                Haptics.tap()
                pool.reconnectNow()
            }
            .font(.system(size: 12, weight: .medium))
            .foregroundStyle(WaystoneTheme.textPrimary)
            .padding(.horizontal, 12)
            .frame(height: 28)
            .background(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .fill(WaystoneTheme.surfaceRaised)
                    .overlay(
                        RoundedRectangle(cornerRadius: 5, style: .continuous)
                            .strokeBorder(WaystoneTheme.border, lineWidth: 0.75)
                    )
            )
            .buttonStyle(ConsoleButtonStyle())
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 13)
    }

    private var hairline: some View {
        Rectangle()
            .fill(WaystoneTheme.borderSubtle)
            .frame(height: 0.75)
    }
}

/// Concise server connection status — plain text, no redundant dots.
private struct PoolStatusLabel: View {
    var status: PoolConnection.Status
    var retry: () -> Void

    var body: some View {
        switch status {
        case .online:
            Text("在线")
                .font(.system(size: 11, weight: .regular, design: .monospaced))
                .foregroundStyle(WaystoneTheme.textTertiary)
        case .connecting:
            HStack(spacing: 5) {
                ProgressView()
                    .controlSize(.mini)
                    .tint(WaystoneTheme.textSecondary)
                Text("连接中")
                    .font(.system(size: 11, weight: .regular, design: .monospaced))
                    .foregroundStyle(WaystoneTheme.textSecondary)
            }
        case .offline(let reason):
            Button(action: retry) {
                Text(reason)
                    .font(.system(size: 11, weight: .regular, design: .monospaced))
                    .foregroundStyle(WaystoneTheme.amber)
                    .lineLimit(1)
            }
            .buttonStyle(ConsoleButtonStyle())
        case .rejected:
            Text("已移除")
                .font(.system(size: 11, weight: .regular, design: .monospaced))
                .foregroundStyle(WaystoneTheme.coral)
        }
    }
}

/// One machine row inside the full-bleed ledger: zero redundant status dots.
private struct HostRow: View {
    var host: Host
    var reachable: Bool
    var connect: () -> Void

    private var available: Bool { reachable && host.online && !host.busy }

    var body: some View {
        Button(action: connect) {
            HStack(spacing: 12) {
                Image(systemName: symbol)
                    .font(.system(size: 15.5, weight: .medium))
                    .foregroundStyle(available ? WaystoneTheme.textPrimary : WaystoneTheme.textTertiary)
                    .frame(width: 22, alignment: .center)

                VStack(alignment: .leading, spacing: 2.5) {
                    Text(host.name)
                        .font(.system(size: 15, weight: available ? .semibold : .medium))
                        .foregroundStyle(available ? WaystoneTheme.textPrimary : WaystoneTheme.textSecondary)
                        .lineLimit(1)

                    Text(subtitleText)
                        .font(.system(size: 11, weight: .regular, design: .monospaced))
                        .foregroundStyle(subtitleColor)
                        .lineLimit(1)
                }

                Spacer(minLength: 10)

                if available {
                    HStack(spacing: 10) {
                        if let rtt = host.rttMs {
                            Text("\(rtt) ms")
                                .font(.system(size: 12, weight: .medium, design: .monospaced))
                                .monospacedDigit()
                                .foregroundStyle(WaystoneTheme.latencyColor(for: rtt))
                        }
                        Text("→")
                            .font(.system(size: 13, weight: .regular, design: .monospaced))
                            .foregroundStyle(WaystoneTheme.textTertiary)
                    }
                } else {
                    Text("—")
                        .font(.system(size: 12, weight: .regular, design: .monospaced))
                        .foregroundStyle(WaystoneTheme.textQuaternary)
                }
            }
            .padding(.horizontal, 18)
            .padding(.vertical, 14)
            .contentShape(Rectangle())
        }
        .buttonStyle(ConsoleButtonStyle())
        .disabled(!available)
        // Label must start with `host.name` for UI tests and VoiceOver scan order.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("\(host.name)，\(accessibilitySubtitle)")
        .accessibilityHint(available ? "开始串流" : accessibilitySubtitle)
        .accessibilityAddTraits(.isButton)
    }

    private var subtitleColor: Color {
        if !reachable || !host.online { return WaystoneTheme.textTertiary }
        if host.busy { return WaystoneTheme.amber }
        return WaystoneTheme.textTertiary
    }

    private var subtitleText: String {
        guard reachable else { return "服务器未连接" }
        guard host.online else { return "离线" }
        if host.busy { return "占用中" }
        return platformName ?? "在线"
    }

    private var platformName: String? {
        switch host.platform {
        case .macos: return "macOS"
        case .windows: return "Windows"
        case .linux: return "Linux"
        default: return nil
        }
    }

    private var accessibilitySubtitle: String {
        guard reachable else { return "服务器未连接" }
        guard host.online else { return "离线" }
        if host.busy { return "占用中" }
        if let rtt = host.rttMs { return "在线，延迟 \(rtt) 毫秒" }
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
        VStack(alignment: .leading, spacing: 0) {
            Spacer()

            Text("暂无服务器")
                .font(.system(size: 20, weight: .semibold))
                .foregroundStyle(WaystoneTheme.textPrimary)
                .padding(.bottom, 18)

            VStack(spacing: 0) {
                Rectangle().fill(WaystoneTheme.borderSubtle).frame(height: 0.75)
                stepRow("1", text: "在 Waystone 面板点击「添加我的设备」")
                Rectangle().fill(WaystoneTheme.borderSubtle).frame(height: 0.75)
                stepRow("2", text: "扫码或粘贴配对链接")
                Rectangle().fill(WaystoneTheme.borderSubtle).frame(height: 0.75)
            }
            .padding(.bottom, 22)

            Button(action: add) {
                HStack(spacing: 8) {
                    Image(systemName: "qrcode.viewfinder")
                        .font(.system(size: 15, weight: .semibold))
                    Text("扫码添加")
                        .font(.system(size: 14, weight: .semibold))
                }
                .foregroundStyle(WaystoneTheme.accentInk)
                .frame(maxWidth: .infinity)
                .frame(height: 44)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(WaystoneTheme.accent)
                )
            }
            .buttonStyle(ConsoleButtonStyle())

            Spacer()
            Spacer()
        }
        .padding(.horizontal, 24)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func stepRow(_ index: String, text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(index)
                .font(.system(size: 11, weight: .regular, design: .monospaced))
                .foregroundStyle(WaystoneTheme.textTertiary)
            Text(text)
                .font(.system(size: 12.5))
                .foregroundStyle(WaystoneTheme.textSecondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .padding(.vertical, 12)
    }
}

struct NoticeCard: View {
    var symbol: String?
    var title: String
    var message: String?

    var body: some View {
        HStack(alignment: .center, spacing: 10) {
            if let symbol {
                Image(systemName: symbol)
                    .font(.system(size: 14, weight: .medium))
                    .foregroundStyle(WaystoneTheme.amber)
                    .frame(width: 20)
            } else {
                ProgressView()
                    .controlSize(.small)
                    .tint(WaystoneTheme.textPrimary)
                    .frame(width: 20)
            }

            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(WaystoneTheme.textPrimary)
                if let message {
                    Text(message)
                        .font(.system(size: 12))
                        .foregroundStyle(WaystoneTheme.textSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 11)
        .frame(maxWidth: 460)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(WaystoneTheme.surfaceRaised)
                .overlay(
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .strokeBorder(WaystoneTheme.borderStrong, lineWidth: 0.75)
                )
        )
        .transition(.move(edge: .bottom).combined(with: .opacity))
    }
}

/// App-level settings sheet opened from the home screen top bar.
private struct SettingsSheet: View {
    @ObservedObject var preferences: StreamPreferences
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("设置")
                    .font(.system(size: 16, weight: .semibold))
                    .foregroundStyle(WaystoneTheme.textPrimary)
                Spacer()
                Button("完成") {
                    Haptics.tap()
                    dismiss()
                }
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(WaystoneTheme.textSecondary)
                .buttonStyle(ConsoleButtonStyle())
            }
            .padding(.horizontal, 18)
            .padding(.top, 20)
            .padding(.bottom, 14)

            Rectangle()
                .fill(WaystoneTheme.borderSubtle)
                .frame(height: 0.75)

            VStack(alignment: .leading, spacing: 8) {
                Text("电源与显示")
                    .font(.system(size: 11.5, weight: .semibold, design: .monospaced))
                    .foregroundStyle(WaystoneTheme.textSecondary)
                    .padding(.horizontal, 18)
                    .padding(.top, 18)

                VStack(spacing: 0) {
                    Rectangle().fill(WaystoneTheme.borderSubtle).frame(height: 0.75)

                    HStack(alignment: .center, spacing: 14) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text("防止自动休眠")
                                .font(.system(size: 14.5, weight: .medium))
                                .foregroundStyle(WaystoneTheme.textPrimary)
                            Text("应用在前台时保持屏幕常亮，避免串流或等待时自动锁屏。")
                                .font(.system(size: 12))
                                .foregroundStyle(WaystoneTheme.textSecondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }

                        Spacer(minLength: 8)

                        Toggle("", isOn: $preferences.preventSleep)
                            .labelsHidden()
                            .tint(WaystoneTheme.emerald)
                    }
                    .padding(.horizontal, 18)
                    .padding(.vertical, 14)

                    Rectangle().fill(WaystoneTheme.borderSubtle).frame(height: 0.75)
                }
                .background(WaystoneTheme.ledger)
            }

            Spacer(minLength: 0)
        }
        .background(WaystoneTheme.canvas.ignoresSafeArea())
    }
}
