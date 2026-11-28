import AppKit
import SwiftUI

struct NotificationsStageView: View {
    @EnvironmentObject private var model: LauncherModel

    var body: some View {
        GlassSurface(cornerRadius: 20, material: .hudWindow, tintOpacity: 0.06) {
            VStack(alignment: .leading, spacing: 14) {
                HStack(spacing: 10) {
                    Image(systemName: "bell.badge.fill")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundColor(.blue)
                    Text("iPhone Notifications")
                        .font(.system(size: 16, weight: .bold))
                        .foregroundColor(.white)
                    Spacer()
                    Toggle("", isOn: $model.notificationsEnabled)
                        .labelsHidden()
                        .toggleStyle(.switch)
                        .onChange(of: model.notificationsEnabled) { _ in
                            model.handleNotificationsEnabledChanged()
                        }
                    Button {
                        withAnimation(.easeInOut(duration: 0.16)) {
                            model.showNotificationsPanel = false
                        }
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 11, weight: .bold))
                            .foregroundColor(Color.white.opacity(0.75))
                            .frame(width: 24, height: 24)
                            .background(Circle().fill(Color.white.opacity(0.10)))
                    }
                    .buttonStyle(.plain)
                }

                // BLE / Relay Status Card
                VStack(alignment: .leading, spacing: 8) {
                    HStack(spacing: 8) {
                        Circle()
                            .fill(model.bleAncsState.isHealthy ? Color.green : Color.orange)
                            .frame(width: 8, height: 8)
                        Text(model.bleAncsState.statusText)
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundColor(.white)
                        Spacer()
                        if model.bleAncsState == .poweredOff {
                            Button("Bluetooth Settings") {
                                if let url = URL(string: "x-apple.systempreferences:com.apple.BluetoothSettings") {
                                    NSWorkspace.shared.open(url)
                                }
                            }
                            .font(.system(size: 11, weight: .semibold))
                        } else {
                            Button("Reconnect BLE") {
                                model.reconnectBleAncs()
                            }
                            .font(.system(size: 11, weight: .semibold))
                        }
                    }

                    Text("1. Pair your iPhone with this Mac over Bluetooth. 2. On iPhone open Settings > Bluetooth > (i) > Share System Notifications.")
                        .font(.system(size: 11))
                        .foregroundColor(Color.white.opacity(0.65))

                    HStack(spacing: 10) {
                        Button("Send Test Notification") {
                            model.sendTestNotification()
                        }
                        .font(.system(size: 11.5, weight: .semibold))

                        Button("Sync App Icons (USB/Wi-Fi)") {
                            Task { await model.syncAppIcons() }
                        }
                        .font(.system(size: 11.5))

                        Spacer()

                        if !model.notificationHistory.isEmpty {
                            Button("Clear History") {
                                model.notificationHistory.removeAll()
                                model.unreadNotificationCount = 0
                            }
                            .font(.system(size: 11))
                        }
                    }
                }
                .padding(12)
                .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.white.opacity(0.06)))
                .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.white.opacity(0.10), lineWidth: 1))

                // Quick Preferences
                HStack(spacing: 14) {
                    Toggle("Keep in Menu Bar", isOn: $model.keepInMenuBar)
                        .onChange(of: model.keepInMenuBar) { _ in model.syncNotificationSettings() }
                    Toggle("Mute while Mirroring", isOn: $model.muteWhileMirroring)
                        .onChange(of: model.muteWhileMirroring) { _ in model.syncNotificationSettings() }
                    Toggle("Hide Previews", isOn: $model.hideNotificationPreviews)
                        .onChange(of: model.hideNotificationPreviews) { _ in model.syncNotificationSettings() }
                }
                .font(.system(size: 11.5))
                .foregroundColor(Color.white.opacity(0.85))
                .toggleStyle(.checkbox)

                if model.relayEnabled, let token = model.relayToken {
                    HStack(spacing: 8) {
                        Image(systemName: "network")
                            .font(.system(size: 11))
                            .foregroundColor(.blue)
                        Text("Local Relay: http://127.0.0.1:\(model.relayPort)/notify")
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundColor(Color.white.opacity(0.75))
                        Spacer()
                        Button("Copy Bearer Token") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(token, forType: .string)
                        }
                        .font(.system(size: 11))
                    }
                    .padding(8)
                    .background(RoundedRectangle(cornerRadius: 9, style: .continuous).fill(Color.black.opacity(0.22)))
                }

                // Notification Feed
                if model.notificationHistory.isEmpty {
                    VStack(spacing: 8) {
                        Spacer()
                        Image(systemName: "bell.slash")
                            .font(.system(size: 28, weight: .light))
                            .foregroundColor(Color.white.opacity(0.42))
                        Text("No notifications yet")
                            .font(.system(size: 14, weight: .semibold))
                            .foregroundColor(Color.white.opacity(0.78))
                        Text("Click “Send Test Notification” above to test native macOS banners right away.")
                            .font(.system(size: 11.5))
                            .foregroundColor(Color.white.opacity(0.55))
                        Spacer()
                    }
                    .frame(maxWidth: .infinity)
                } else {
                    ScrollView {
                        LazyVStack(spacing: 8) {
                            ForEach(model.notificationHistory) { notif in
                                notificationRow(notif)
                            }
                        }
                    }
                }
            }
            .padding(20)
        }
    }

    private func notificationRow(_ notif: IosNotificationPayload) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Group {
                if let path = notif.icon_png_path, let img = NSImage(contentsOfFile: path) {
                    Image(nsImage: img)
                        .resizable()
                        .aspectRatio(contentMode: .fill)
                } else {
                    Image(systemName: notif.category == "incoming_call" ? "phone.fill" : "app.badge")
                        .font(.system(size: 14))
                        .foregroundColor(.white.opacity(0.85))
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background(Color.blue.opacity(0.28))
                }
            }
            .frame(width: 32, height: 32)
            .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))

            VStack(alignment: .leading, spacing: 2) {
                HStack {
                    Text(notif.app_display_name)
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundColor(.white.opacity(0.65))
                    if let sub = notif.subtitle, !sub.isEmpty {
                        Text("• \(sub)")
                            .font(.system(size: 11))
                            .foregroundColor(.white.opacity(0.5))
                            .lineLimit(1)
                    }
                }
                Text(notif.title)
                    .font(.system(size: 12.5, weight: .semibold))
                    .foregroundColor(.white)
                    .lineLimit(1)
                if !notif.body.isEmpty {
                    Text(notif.body)
                        .font(.system(size: 11.5))
                        .foregroundColor(.white.opacity(0.75))
                        .lineLimit(2)
                }
            }
            Spacer()
            HStack(spacing: 6) {
                Button("Open") {
                    model.openBundleInMirror(notif.app_bundle_id)
                }
                .font(.system(size: 11, weight: .semibold))
                Button {
                    model.dismissNotification(notif)
                } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 10, weight: .semibold))
                }
                .buttonStyle(.plain)
                .foregroundColor(.white.opacity(0.6))
            }
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Color.white.opacity(0.06)))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(Color.white.opacity(0.09), lineWidth: 1))
    }
}
