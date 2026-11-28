import AppKit
import Foundation

@MainActor
final class MenuBarController: NSObject {
    private var statusItem: NSStatusItem?
    private weak var model: LauncherModel?
    private var openLauncherHandler: (() -> Void)?

    func setup(model: LauncherModel, onOpenLauncher: @escaping () -> Void) {
        self.model = model
        self.openLauncherHandler = onOpenLauncher
        if statusItem == nil {
            let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
            if let button = item.button {
                button.image = NSImage(systemSymbolName: "iphone.gen3", accessibilityDescription: "iPhone Mirror")
                button.image?.isTemplate = true
            }
            self.statusItem = item
        }
        rebuildMenu()
    }

    func tearDown() {
        if let item = statusItem {
            NSStatusBar.system.removeStatusItem(item)
            statusItem = nil
        }
    }

    func rebuildMenu() {
        guard let item = statusItem, let model else { return }
        if let button = item.button {
            let count = model.unreadNotificationCount
            button.title = count > 0 ? " \(count)" : ""
        }

        let menu = NSMenu()
        let titleItem = NSMenuItem(title: "iPhone Mirror • \(model.bleAncsState.statusText)", action: nil, keyEquivalent: "")
        titleItem.isEnabled = false
        menu.addItem(titleItem)

        let toggleDnd = NSMenuItem(
            title: model.notificationsPaused ? "Resume iPhone Notifications" : "Pause iPhone Notifications",
            action: #selector(togglePause),
            keyEquivalent: ""
        )
        toggleDnd.target = self
        menu.addItem(toggleDnd)

        let testItem = NSMenuItem(title: "Send Test Notification", action: #selector(sendTest), keyEquivalent: "")
        testItem.target = self
        menu.addItem(testItem)

        menu.addItem(.separator())

        if model.notificationHistory.isEmpty {
            let empty = NSMenuItem(title: "No recent iPhone notifications", action: nil, keyEquivalent: "")
            empty.isEnabled = false
            menu.addItem(empty)
        } else {
            for notif in model.notificationHistory.prefix(8) {
                let summary = "\(notif.app_display_name): \(notif.title)"
                let row = NSMenuItem(title: summary, action: #selector(openFromMenu(_:)), keyEquivalent: "")
                row.target = self
                row.representedObject = notif.app_bundle_id
                menu.addItem(row)
            }
        }

        menu.addItem(.separator())

        let openLauncher = NSMenuItem(title: "Open iPhone Mirror", action: #selector(openLauncher), keyEquivalent: "")
        openLauncher.target = self
        menu.addItem(openLauncher)

        let mirrorToggle = NSMenuItem(
            title: model.backendRunning ? "Stop Mirroring" : "Start Mirroring",
            action: #selector(toggleMirroring),
            keyEquivalent: ""
        )
        mirrorToggle.target = self
        menu.addItem(mirrorToggle)

        menu.addItem(.separator())

        let quitItem = NSMenuItem(title: "Quit iPhone Mirror", action: #selector(quitApp), keyEquivalent: "q")
        quitItem.target = self
        menu.addItem(quitItem)

        item.menu = menu
    }

    @objc private func togglePause() {
        model?.notificationsPaused.toggle()
        model?.syncNotificationSettings()
        rebuildMenu()
    }

    @objc private func sendTest() {
        model?.sendTestNotification()
    }

    @objc private func openFromMenu(_ sender: NSMenuItem) {
        if let bundleID = sender.representedObject as? String, !bundleID.isEmpty {
            model?.openBundleInMirror(bundleID)
        } else {
            openLauncherHandler?()
        }
    }

    @objc private func openLauncher() {
        openLauncherHandler?()
    }

    @objc private func toggleMirroring() {
        guard let model else { return }
        if model.backendRunning {
            model.disconnect()
        } else {
            model.connect()
        }
    }

    @objc private func quitApp() {
        model?.prepareForExplicitQuit()
        NSApp.terminate(nil)
    }
}
