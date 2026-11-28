import Foundation

struct NotificationSettingsPayload: Codable, Equatable {
    var enabled: Bool = true
    var keep_in_menu_bar: Bool = true
    var mute_while_mirroring: Bool = true
    var hide_previews: Bool = false
    var muted_bundle_ids: [String] = []
    var relay_enabled: Bool = false
    var relay_port: UInt16 = 48125
    var relay_token: String = ""
}

private enum DaemonEventDecoder: Decodable {
    case writeControlPoint(bytes_b64: String)
    case deliverNotification(notification: IosNotificationPayload)
    case recordHistoryOnly(notification: IosNotificationPayload)
    case retractNotification(uid: UInt32)
    case relayReady(port: UInt16, token: String, lan_url: String?)
    case status(message: String)

    private enum CodingKeys: String, CodingKey {
        case type
        case bytes_b64
        case notification
        case uid
        case port
        case token
        case lan_url
        case message
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "write_control_point":
            self = .writeControlPoint(bytes_b64: try c.decode(String.self, forKey: .bytes_b64))
        case "deliver_notification":
            self = .deliverNotification(notification: try c.decode(IosNotificationPayload.self, forKey: .notification))
        case "record_history_only":
            self = .recordHistoryOnly(notification: try c.decode(IosNotificationPayload.self, forKey: .notification))
        case "retract_notification":
            self = .retractNotification(uid: try c.decode(UInt32.self, forKey: .uid))
        case "relay_ready":
            self = .relayReady(
                port: try c.decode(UInt16.self, forKey: .port),
                token: try c.decode(String.self, forKey: .token),
                lan_url: try? c.decode(String.self, forKey: .lan_url)
            )
        default:
            let msg = (try? c.decode(String.self, forKey: .message)) ?? type
            self = .status(message: msg)
        }
    }
}

final class NotificationDaemonBridge {
    var onWriteControlPoint: ((Data) -> Void)?
    var onDeliverNotification: ((IosNotificationPayload) -> Void)?
    var onRecordHistoryOnly: ((IosNotificationPayload) -> Void)?
    var onRetractNotification: ((UInt32) -> Void)?
    var onRelayReady: ((UInt16, String, String?) -> Void)?
    var onStatus: ((String) -> Void)?

    private var process: Process?
    private var stdinHandle: FileHandle?
    private var lineBuffer = Data()
    private let backendURL: URL

    init(backendURL: URL) {
        self.backendURL = backendURL
    }

    deinit {
        stop()
    }

    func start() {
        guard process == nil else { return }
        let proc = Process()
        proc.executableURL = backendURL
        proc.arguments = ["notify-daemon"]

        let inPipe = Pipe()
        let outPipe = Pipe()
        proc.standardInput = inPipe
        proc.standardOutput = outPipe
        proc.standardError = FileHandle.nullDevice

        outPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            if chunk.isEmpty { return }
            DispatchQueue.main.async {
                self?.consumeStdout(chunk)
            }
        }

        proc.terminationHandler = { [weak self] _ in
            outPipe.fileHandleForReading.readabilityHandler = nil
            DispatchQueue.main.async {
                self?.process = nil
                self?.stdinHandle = nil
            }
        }

        do {
            try proc.run()
            self.process = proc
            self.stdinHandle = inPipe.fileHandleForWriting
        } catch {
            onStatus?("Failed to start notify-daemon: \(error.localizedDescription)")
        }
    }

    func stop() {
        stdinHandle = nil
        process?.terminate()
        process = nil
    }

    func sendBleConnected(name: String) {
        sendJSON(["type": "ble_connected", "peripheral_name": name])
    }

    func sendBleDisconnected(reason: String) {
        sendJSON(["type": "ble_disconnected", "reason": reason])
    }

    func sendBleNotificationSource(_ data: Data) {
        sendJSON(["type": "ble_notification_source", "bytes_b64": data.base64EncodedString()])
    }

    func sendBleDataSource(_ data: Data) {
        sendJSON(["type": "ble_data_source", "bytes_b64": data.base64EncodedString()])
    }

    func sendPerformAction(uid: UInt32, positive: Bool) {
        sendJSON(["type": "perform_action", "uid": uid, "positive": positive])
    }

    func sendSetMirrorActive(_ active: Bool) {
        sendJSON(["type": "set_mirror_active", "active": active])
    }

    func sendTestNotification() {
        sendJSON(["type": "send_test_notification"])
    }

    func sendUpdateSettings(_ settings: NotificationSettingsPayload) {
        struct Envelope: Encodable {
            let type = "update_settings"
            let settings: NotificationSettingsPayload
        }
        guard let data = try? JSONEncoder().encode(Envelope(settings: settings)) else { return }
        writeLine(data)
    }

    private func sendJSON(_ dict: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: dict) else { return }
        writeLine(data)
    }

    private func writeLine(_ jsonData: Data) {
        if process == nil {
            start()
        }
        guard let handle = stdinHandle else { return }
        var line = jsonData
        line.append(0x0A)
        try? handle.write(contentsOf: line)
    }

    private func consumeStdout(_ chunk: Data) {
        lineBuffer.append(chunk)
        while let newlineIndex = lineBuffer.firstIndex(of: 0x0A) {
            let line = lineBuffer.subdata(in: lineBuffer.startIndex..<newlineIndex)
            lineBuffer.removeSubrange(lineBuffer.startIndex...newlineIndex)
            guard !line.isEmpty,
                  let decoded = try? JSONDecoder().decode(DaemonEventDecoder.self, from: line) else {
                continue
            }
            switch decoded {
            case .writeControlPoint(let b64):
                if let data = Data(base64Encoded: b64) {
                    onWriteControlPoint?(data)
                }
            case .deliverNotification(let notif):
                onDeliverNotification?(notif)
            case .recordHistoryOnly(let notif):
                onRecordHistoryOnly?(notif)
            case .retractNotification(let uid):
                onRetractNotification?(uid)
            case .relayReady(let port, let token, let lanUrl):
                onRelayReady?(port, token, lanUrl)
            case .status(let message):
                onStatus?(message)
            }
        }
    }
}
