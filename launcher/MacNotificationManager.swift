import AppKit
import Foundation
import UserNotifications

struct NotificationFlagsPayload: Codable, Hashable {
    var silent: Bool = false
    var important: Bool = false
    var pre_existing: Bool = false
    var positive_action: Bool = false
    var negative_action: Bool = false
}

struct IosNotificationPayload: Codable, Identifiable, Hashable {
    let uid: UInt32
    let event_kind: String
    let flags: NotificationFlagsPayload
    let category: String
    let app_bundle_id: String
    let app_display_name: String
    let title: String
    let subtitle: String?
    let body: String
    let date: String?
    let positive_label: String?
    let negative_label: String?
    let icon_png_path: String?

    var id: UInt32 { uid }
}

final class MacNotificationManager: NSObject, UNUserNotificationCenterDelegate {
    static let callCategoryID = "ANCS_INCOMING_CALL"
    static let actionableCategoryID = "ANCS_ACTIONABLE"
    static let readonlyCategoryID = "ANCS_READONLY"

    static let answerActionID = "ANCS_ACTION_POSITIVE"
    static let declineActionID = "ANCS_ACTION_NEGATIVE"
    static let openAppActionID = "ANCS_ACTION_OPEN_APP"
    static let dismissActionID = "ANCS_ACTION_DISMISS"

    var onPerformAncsAction: ((UInt32, Bool) -> Void)?
    var onOpenBundleInMirror: ((String) -> Void)?
    var onAuthorizationUpdated: ((Bool, String?) -> Void)?

    private var authorized = false

    func configureAndRequestAuthorization() {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        registerCategories(on: center)

        center.requestAuthorization(options: [.alert, .sound, .badge]) { [weak self] granted, error in
            Task { @MainActor in
                self?.authorized = granted
                self?.onAuthorizationUpdated?(granted, error?.localizedDescription)
            }
        }
    }

    private func registerCategories(on center: UNUserNotificationCenter) {
        let answer = UNNotificationAction(
            identifier: Self.answerActionID,
            title: "Answer",
            options: [.foreground]
        )
        let decline = UNNotificationAction(
            identifier: Self.declineActionID,
            title: "Decline",
            options: [.destructive]
        )
        let callCategory = UNNotificationCategory(
            identifier: Self.callCategoryID,
            actions: [answer, decline],
            intentIdentifiers: [],
            options: [.customDismissAction]
        )

        let openApp = UNNotificationAction(
            identifier: Self.openAppActionID,
            title: "Open in iPhone Mirror",
            options: [.foreground]
        )
        let dismiss = UNNotificationAction(
            identifier: Self.dismissActionID,
            title: "Dismiss on iPhone",
            options: []
        )
        let actionableCategory = UNNotificationCategory(
            identifier: Self.actionableCategoryID,
            actions: [openApp, dismiss],
            intentIdentifiers: [],
            options: [.customDismissAction]
        )

        let readonlyCategory = UNNotificationCategory(
            identifier: Self.readonlyCategoryID,
            actions: [openApp],
            intentIdentifiers: [],
            options: [.customDismissAction]
        )

        center.setNotificationCategories([callCategory, actionableCategory, readonlyCategory])
    }

    func deliver(_ notif: IosNotificationPayload) {
        let center = UNUserNotificationCenter.current()
        let content = UNMutableNotificationContent()

        if notif.title.caseInsensitiveCompare(notif.app_display_name) == .orderedSame || notif.title.isEmpty {
            content.title = notif.app_display_name
        } else {
            content.title = "\(notif.app_display_name) • \(notif.title)"
        }
        if let sub = notif.subtitle, !sub.isEmpty {
            content.subtitle = sub
        }
        content.body = notif.body.isEmpty ? "New notification" : notif.body
        content.threadIdentifier = notif.app_bundle_id
        content.sound = notif.flags.silent ? nil : .default

        if notif.category == "incoming_call" {
            content.categoryIdentifier = Self.callCategoryID
        } else if notif.flags.negative_action || notif.flags.positive_action {
            content.categoryIdentifier = Self.actionableCategoryID
        } else {
            content.categoryIdentifier = Self.readonlyCategoryID
        }

        content.userInfo = [
            "uid": Int(notif.uid),
            "bundle_id": notif.app_bundle_id,
        ]

        // UNNotificationAttachment moves the file it is given, so copy from our persistent cache.
        if let iconPath = notif.icon_png_path, FileManager.default.fileExists(atPath: iconPath) {
            let tempIcon = FileManager.default.temporaryDirectory
                .appendingPathComponent("ancs-icon-\(notif.uid)-\(UUID().uuidString).png")
            if (try? FileManager.default.copyItem(at: URL(fileURLWithPath: iconPath), to: tempIcon)) != nil,
               let attachment = try? UNNotificationAttachment(
                   identifier: "app-icon-\(notif.uid)",
                   url: tempIcon,
                   options: nil
               ) {
                content.attachments = [attachment]
            }
        }

        let request = UNNotificationRequest(
            identifier: Self.identifier(for: notif.uid),
            content: content,
            trigger: nil
        )
        center.add(request) { error in
            if let error {
                NSLog("MacNotificationManager delivery error: \(error.localizedDescription)")
            }
        }
    }

    func retract(uid: UInt32) {
        let id = Self.identifier(for: uid)
        let center = UNUserNotificationCenter.current()
        center.removeDeliveredNotifications(withIdentifiers: [id])
        center.removePendingNotificationRequests(withIdentifiers: [id])
    }

    private static func identifier(for uid: UInt32) -> String {
        "ancs-\(uid)"
    }

    // Show banner + list even while launcher window is active/foreground.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        var options: UNNotificationPresentationOptions = [.banner, .list]
        if notification.request.content.sound != nil {
            options.insert(.sound)
        }
        completionHandler(options)
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let info = response.notification.request.content.userInfo
        let uid = (info["uid"] as? NSNumber)?.uint32Value ?? 0
        let bundleID = info["bundle_id"] as? String ?? ""

        switch response.actionIdentifier {
        case Self.answerActionID:
            if uid > 0 { onPerformAncsAction?(uid, true) }
            if !bundleID.isEmpty { onOpenBundleInMirror?(bundleID) }
        case Self.declineActionID, Self.dismissActionID, UNNotificationDismissActionIdentifier:
            if uid > 0 { onPerformAncsAction?(uid, false) }
        case Self.openAppActionID, UNNotificationDefaultActionIdentifier:
            if !bundleID.isEmpty {
                onOpenBundleInMirror?(bundleID)
            }
        default:
            break
        }
        completionHandler()
    }
}
