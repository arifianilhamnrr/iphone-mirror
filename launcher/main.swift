import AppKit
import Network
import SwiftUI

private enum AppInfo {
    static let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.3.0"
    static let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "1"
    static let buildNumber = Int(build) ?? 1
    static let isBeta = buildNumber > 1 || version.lowercased().contains("beta")

    static var label: String {
        if !isBeta {
            return "v\(version)"
        }
        let base = version.lowercased().contains("beta") ? version : "\(version)-beta"
        return "v\(base) (build \(build))"
    }

    static func parseSemver(_ tag: String) -> [Int] {
        let cleaned = tag
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .replacingOccurrences(of: "^v", with: "", options: .regularExpression)
            .split(separator: "-", maxSplits: 1)
            .first
            .map(String.init) ?? ""
        return cleaned.split(separator: ".").prefix(3).map { Int($0) ?? 0 }
    }

    static func isNewerVersion(remoteTag: String, than localVersion: String, localIsBeta: Bool) -> Bool {
        let remoteParts = parseSemver(remoteTag)
        let localParts = parseSemver(localVersion)
        let count = max(remoteParts.count, localParts.count, 3)
        for i in 0..<count {
            let r = i < remoteParts.count ? remoteParts[i] : 0
            let l = i < localParts.count ? localParts[i] : 0
            if r != l {
                return r > l
            }
        }
        let remoteIsPrerelease = remoteTag.lowercased().contains("beta")
            || remoteTag.lowercased().contains("alpha")
            || remoteTag.lowercased().contains("rc")
        return localIsBeta && !remoteIsPrerelease
    }
}

struct GitHubRelease: Decodable {
    let tag_name: String
    let name: String?
    let html_url: String
}

struct AvailableUpdate: Equatable {
    let version: String
    let url: URL
}

struct Device: Codable, Identifiable, Hashable {
    let udid: String
    let connection_type: ConnectionType
    let device_id: UInt32
    let host: String?
    let port: UInt16?
    let name: String?
    let model: String?

    var id: String { "\(udid)-\(connection_type.id)" }
    var isRemote: Bool { host != nil && port != nil }

    private static func isUUIDLike(_ value: String) -> Bool {
        let stripped = value.replacingOccurrences(of: "-", with: "").replacingOccurrences(of: ":", with: "")
        return stripped.count >= 16 && stripped.allSatisfy { $0.isHexDigit }
    }

    var resolvedUserName: String {
        if let name = name?.trimmingCharacters(in: .whitespacesAndNewlines),
           !name.isEmpty,
           !Self.isUUIDLike(name) {
            return name
        }
        if let host = host?.trimmingCharacters(in: CharacterSet(charactersIn: ".")) {
            let shortHost = host.hasSuffix(".local") ? String(host.dropLast(6)) : host
            if !shortHost.isEmpty, !Self.isUUIDLike(shortHost) {
                return shortHost
            }
        }
        return connection_type == .usb ? "iPhone (USB)" : "iPhone (Wi-Fi)"
    }

    var displayName: String {
        if let model = model, !model.isEmpty {
            return "\(resolvedUserName) · \(model)"
        }
        return resolvedUserName
    }
}

struct NearbyDevice: Identifiable, Hashable {
    let name: String
    let service: String
    var id: String { "\(service)-\(name)" }

    var displayName: String {
        let compact = name.replacingOccurrences(of: "-", with: "")
        if compact.count >= 24 && compact.allSatisfy({ $0.isHexDigit }) {
            return "Nearby iPhone"
        }
        return name
    }
}

enum ConnectionType: Codable, Hashable {
    case usb
    case network
    case unknown(String)

    var id: String {
        switch self {
        case .usb: return "usb"
        case .network: return "network"
        case .unknown(let value): return value
        }
    }

    var label: String {
        switch self {
        case .usb: return "USB"
        case .network: return "Wi-Fi"
        case .unknown: return "Other"
        }
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let value = try? container.decode(String.self) {
            self = value == "usb" ? .usb : value == "network" ? .network : .unknown(value)
            return
        }
        let object = try container.decode([String: String].self)
        self = .unknown(object["unknown"] ?? "unknown")
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(id)
    }
}

private enum TransportMode: String, CaseIterable, Identifiable {
    case auto
    case usb
    case wifi
    case airplay

    var id: String { rawValue }

    var title: String {
        switch self {
        case .auto: return "Auto"
        case .usb: return "USB"
        case .wifi: return "Wi-Fi"
        case .airplay: return "AirPlay"
        }
    }

    var icon: String {
        switch self {
        case .auto: return "sparkles"
        case .usb: return "cable.connector"
        case .wifi: return "wifi"
        case .airplay: return "airplayvideo"
        }
    }
}

@MainActor
final class LauncherModel: ObservableObject {
    @Published var devices: [Device] = []
    @Published var selectedID: String?
    @Published var isScanning = false
    @Published var isManualScanning = false
    @Published var isPairing = false
    @Published var pairingStatus: String?
    @Published var nearbyDevices: [NearbyDevice] = []
    @Published var bonjourScanning = false
    @Published var error: String?
    @Published var backendRunning = false
    @Published var availableUpdate: AvailableUpdate?
    @Published var updateDismissed = false
    @Published var isCheckingUpdates = false
    @Published var updateStatusMessage: String?

    @Published var showNotificationsPanel = false
    @Published var bleAncsState: BleAncsState = .disabled
    @Published var notificationsAuthorized = false
    @Published var notificationsPaused = false
    @Published var unreadNotificationCount = 0
    @Published var notificationHistory: [IosNotificationPayload] = []
    @Published var relayPort: UInt16 = 48125
    @Published var relayToken: String?
    @Published var explicitQuit = false

    @AppStorage("transport") var transport = "auto"
    @AppStorage("autoRotate") var autoRotate = true
    @AppStorage("autoConnect") var autoConnect = false
    @AppStorage("autoCheckUpdates") var autoCheckUpdates = true
    @AppStorage("notificationsEnabled") var notificationsEnabled = true
    @AppStorage("keepInMenuBar") var keepInMenuBar = true
    @AppStorage("muteWhileMirroring") var muteWhileMirroring = true
    @AppStorage("hideNotificationPreviews") var hideNotificationPreviews = false
    @AppStorage("relayEnabled") var relayEnabled = false
    @AppStorage("mutedBundleIDs") var mutedBundleIDsRaw = ""

    private var backend: Process?
    private var scanTask: Task<Void, Never>?
    private var didAutoConnect = false
    private var browsers: [NWBrowser] = []
    private var nearbyByService: [String: Set<String>] = [:]
    private var notificationManager: MacNotificationManager?
    private var bleAncsClient: CoreBluetoothAncsClient?
    private var daemonBridge: NotificationDaemonBridge?
    private let menuBarController = MenuBarController()
    private var pendingLaunchBundleID: String?

    var backendURL: URL {
        Bundle.main.bundleURL.appendingPathComponent(
            "Contents/Helpers/iPhone Mirror Stream.app/Contents/MacOS/iphone-mirror-backend"
        )
    }

    var usbDevice: Device? {
        devices.first(where: { $0.connection_type == .usb })
    }

    var filteredDevices: [Device] {
        let matches = devices.filter { device in
            transport == "auto"
                || transport == "airplay"
                || (transport == "usb" && device.connection_type == .usb)
                || (transport == "wifi" && device.connection_type == .network)
        }
        var seenUDIDs = Set<String>()
        var seenHosts = Set<String>()
        var seenNames = Set<String>()
        return matches.filter { device in
            let hostKey = device.host?
                .trimmingCharacters(in: CharacterSet(charactersIn: "."))
                .lowercased()
            let nameKey = device.resolvedUserName.lowercased()

            if seenUDIDs.contains(device.udid) {
                return false
            }
            if let hostKey, !hostKey.isEmpty, seenHosts.contains(hostKey) {
                return false
            }
            if (transport == "auto" || transport == "airplay"), seenNames.contains(nameKey) {
                return false
            }
            seenUDIDs.insert(device.udid)
            if let hostKey, !hostKey.isEmpty {
                seenHosts.insert(hostKey)
            }
            seenNames.insert(nameKey)
            return true
        }
    }

    var uniqueNearbyDevices: [NearbyDevice] {
        var seen = Set<String>()
        return nearbyDevices.filter { item in
            let key = item.displayName.lowercased()
            if seen.contains(key) {
                return false
            }
            seen.insert(key)
            return true
        }
    }

    var selected: Device? {
        filteredDevices.first(where: { $0.id == selectedID }) ?? filteredDevices.first
    }

    var canStart: Bool {
        transport == "airplay" || selected != nil
    }

    func start() {
        setupNotificationsAndMenuBar()
        guard scanTask == nil else { return }
        startBonjourScanning()
        if autoCheckUpdates {
            Task { await checkForUpdates(silent: true) }
        }
        scanTask = Task {
            while !Task.isCancelled {
                await scan(manual: false)
                try? await Task.sleep(nanoseconds: 4_000_000_000)
            }
        }
    }

    private func setupNotificationsAndMenuBar() {
        if notificationManager == nil {
            let mgr = MacNotificationManager()
            mgr.onAuthorizationUpdated = { [weak self] granted, _ in
                self?.notificationsAuthorized = granted
            }
            mgr.onPerformAncsAction = { [weak self] uid, positive in
                self?.daemonBridge?.sendPerformAction(uid: uid, positive: positive)
            }
            mgr.onOpenBundleInMirror = { [weak self] bundleID in
                self?.openBundleInMirror(bundleID)
            }
            mgr.configureAndRequestAuthorization()
            notificationManager = mgr
        }

        if daemonBridge == nil {
            let bridge = NotificationDaemonBridge(backendURL: backendURL)
            bridge.onWriteControlPoint = { [weak self] data in
                self?.bleAncsClient?.writeControlPoint(data)
            }
            bridge.onDeliverNotification = { [weak self] notif in
                guard let self else { return }
                self.recordNotification(notif)
                if !self.notificationsPaused {
                    self.notificationManager?.deliver(notif)
                }
            }
            bridge.onRecordHistoryOnly = { [weak self] notif in
                self?.recordNotification(notif)
            }
            bridge.onRetractNotification = { [weak self] uid in
                self?.notificationManager?.retract(uid: uid)
                self?.notificationHistory.removeAll { $0.uid == uid }
                self?.menuBarController.rebuildMenu()
            }
            bridge.onRelayReady = { [weak self] port, token in
                self?.relayPort = port
                self?.relayToken = token
            }
            bridge.start()
            daemonBridge = bridge
        }

        if bleAncsClient == nil {
            let ble = CoreBluetoothAncsClient()
            ble.onStateChanged = { [weak self] state in
                self?.bleAncsState = state
                self?.menuBarController.rebuildMenu()
            }
            ble.onConnected = { [weak self] name in
                self?.daemonBridge?.sendBleConnected(name: name)
            }
            ble.onDisconnected = { [weak self] reason in
                self?.daemonBridge?.sendBleDisconnected(reason: reason)
            }
            ble.onNotificationSourceData = { [weak self] data in
                self?.daemonBridge?.sendBleNotificationSource(data)
            }
            ble.onDataSourceData = { [weak self] data in
                self?.daemonBridge?.sendBleDataSource(data)
            }
            bleAncsClient = ble
        }

        syncNotificationSettings()
        if notificationsEnabled {
            bleAncsClient?.start()
        }
        menuBarController.setup(model: self) { [weak self] in
            self?.presentMainWindow()
        }
    }

    private func recordNotification(_ notif: IosNotificationPayload) {
        notificationHistory.removeAll { $0.uid == notif.uid }
        notificationHistory.insert(notif, at: 0)
        if notificationHistory.count > 100 {
            notificationHistory = Array(notificationHistory.prefix(100))
        }
        unreadNotificationCount = min(unreadNotificationCount + 1, 99)
        menuBarController.rebuildMenu()
    }

    func handleNotificationsEnabledChanged() {
        syncNotificationSettings()
        if notificationsEnabled {
            bleAncsClient?.start()
        } else {
            bleAncsClient?.stop()
        }
    }

    func reconnectBleAncs() {
        if notificationsEnabled {
            bleAncsClient?.reconnect()
        }
    }

    func sendTestNotification() {
        daemonBridge?.sendTestNotification()
    }

    func dismissNotification(_ notif: IosNotificationPayload) {
        notificationManager?.retract(uid: notif.uid)
        daemonBridge?.sendPerformAction(uid: notif.uid, positive: false)
        notificationHistory.removeAll { $0.uid == notif.uid }
        menuBarController.rebuildMenu()
    }

    func syncNotificationSettings() {
        let muted = mutedBundleIDsRaw
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        let payload = NotificationSettingsPayload(
            enabled: notificationsEnabled && !notificationsPaused,
            keep_in_menu_bar: keepInMenuBar,
            mute_while_mirroring: muteWhileMirroring,
            hide_previews: hideNotificationPreviews,
            muted_bundle_ids: muted,
            relay_enabled: relayEnabled,
            relay_port: relayPort,
            relay_token: relayToken ?? ""
        )
        daemonBridge?.sendUpdateSettings(payload)
        if keepInMenuBar {
            menuBarController.setup(model: self) { [weak self] in
                self?.presentMainWindow()
            }
        } else {
            menuBarController.tearDown()
        }
    }

    func openBundleInMirror(_ bundleID: String) {
        if backendRunning {
            NSApp.activate(ignoringOtherApps: true)
            let exec = backendURL
            let udid = selected?.udid
            Task.detached(priority: .userInitiated) {
                let proc = Process()
                proc.executableURL = exec
                proc.arguments = udid != nil
                    ? ["launch-app", "--udid", udid!, bundleID]
                    : ["launch-app", bundleID]
                proc.standardOutput = FileHandle.nullDevice
                proc.standardError = FileHandle.nullDevice
                try? proc.run()
            }
            return
        }
        pendingLaunchBundleID = bundleID
        if canStart {
            connect()
        } else {
            presentMainWindow()
        }
    }

    func syncAppIcons() async {
        let exec = backendURL
        let udid = selected?.udid
        await Task.detached(priority: .utility) {
            let proc = Process()
            proc.executableURL = exec
            proc.arguments = udid != nil ? ["fetch-icons", "--udid", udid!] : ["fetch-icons"]
            proc.standardOutput = FileHandle.nullDevice
            proc.standardError = FileHandle.nullDevice
            try? proc.run()
            proc.waitUntilExit()
        }.value
    }

    func presentMainWindow() {
        Self.showLauncher()
        for win in NSApp.windows where win.canBecomeMain {
            win.makeKeyAndOrderFront(nil)
        }
    }

    func prepareForExplicitQuit() {
        explicitQuit = true
        bleAncsClient?.stop()
        daemonBridge?.stop()
    }

    func checkForUpdates(silent: Bool = false) async {
        guard !isCheckingUpdates else { return }
        isCheckingUpdates = true
        defer { isCheckingUpdates = false }
        if !silent {
            updateStatusMessage = "Checking GitHub releases…"
        }

        guard let url = URL(string: "https://api.github.com/repos/arifianilhamnrr/iphone-mirror/releases/latest") else {
            if !silent { updateStatusMessage = "Invalid update URL." }
            return
        }

        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 10)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("iPhone-Mirror/\(AppInfo.version)", forHTTPHeaderField: "User-Agent")

        do {
            let (data, response) = try await fetchData(for: request)
            if let http = response as? HTTPURLResponse, http.statusCode != 200 {
                if !silent {
                    updateStatusMessage = "Could not check updates (HTTP \(http.statusCode))."
                }
                return
            }
            let release = try JSONDecoder().decode(GitHubRelease.self, from: data)
            let remoteVersion = release.tag_name.trimmingCharacters(in: CharacterSet(charactersIn: "vV "))
            if AppInfo.isNewerVersion(
                remoteTag: release.tag_name,
                than: AppInfo.version,
                localIsBeta: AppInfo.isBeta
            ), let htmlURL = URL(string: release.html_url) {
                availableUpdate = AvailableUpdate(version: remoteVersion, url: htmlURL)
                updateDismissed = false
                updateStatusMessage = "Update available: v\(remoteVersion)"
            } else {
                availableUpdate = nil
                if !silent {
                    updateStatusMessage = "You’re up to date (\(AppInfo.label))."
                }
            }
        } catch {
            if !silent {
                updateStatusMessage = "Update check failed. Check your internet connection."
            }
        }
    }

    func openUpdateDownload() {
        guard let url = availableUpdate?.url else { return }
        NSWorkspace.shared.open(url)
    }

    private func fetchData(for request: URLRequest) async throws -> (Data, URLResponse) {
        try await withCheckedThrowingContinuation { continuation in
            URLSession.shared.dataTask(with: request) { data, response, error in
                if let error {
                    continuation.resume(throwing: error)
                } else if let data, let response {
                    continuation.resume(returning: (data, response))
                } else {
                    continuation.resume(throwing: URLError(.badServerResponse))
                }
            }.resume()
        }
    }

    private func startBonjourScanning() {
        guard browsers.isEmpty else { return }
        bonjourScanning = true
        for service in ["_remotepairing._tcp", "_apple-mobdev2._tcp"] {
            let browser = NWBrowser(
                for: .bonjour(type: service, domain: nil),
                using: .tcp
            )
            browser.stateUpdateHandler = { [weak self] state in
                Task { @MainActor in
                    guard let self else { return }
                    if case .failed = state {
                        self.error = "Wi-Fi discovery stopped. Check Local Network access and try again."
                    }
                }
            }
            browser.browseResultsChangedHandler = { [weak self] results, _ in
                let names = Set(results.compactMap { result -> String? in
                    guard case .service(let name, _, _, _) = result.endpoint else { return nil }
                    return name
                })
                Task { @MainActor in
                    guard let self else { return }
                    self.nearbyByService[service] = names
                    self.nearbyDevices = self.nearbyByService
                        .flatMap { service, names in
                            names.map { NearbyDevice(name: $0, service: service) }
                        }
                        .sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
                }
            }
            browser.start(queue: .global(qos: .utility))
            browsers.append(browser)
        }
    }

    func scan(manual: Bool = false) async {
        if manual {
            isManualScanning = true
        }
        defer {
            if manual {
                isManualScanning = false
            }
        }
        let startedAt = Date()
        let executableURL = backendURL
        do {
            let scanned: [Device] = try await Task.detached(priority: .utility) {
                let process = Process()
                let pipe = Pipe()
                process.executableURL = executableURL
                process.arguments = ["devices", "--json"]
                process.standardOutput = pipe
                process.standardError = Pipe()
                try process.run()
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                process.waitUntilExit()
                guard process.terminationStatus == 0 else {
                    throw NSError(domain: "iPhoneMirror", code: Int(process.terminationStatus), userInfo: nil)
                }
                return try JSONDecoder().decode([Device].self, from: data)
                    .sorted { lhs, rhs in
                        if lhs.connection_type != rhs.connection_type {
                            return lhs.connection_type == .usb
                        }
                        if lhs.udid == rhs.udid {
                            return lhs.connection_type == .usb
                        }
                        return lhs.udid < rhs.udid
                    }
            }.value
            if manual {
                let elapsed = Date().timeIntervalSince(startedAt)
                if elapsed < 0.55 {
                    try? await Task.sleep(nanoseconds: UInt64((0.55 - elapsed) * 1_000_000_000))
                }
            }
            if scanned != devices {
                devices = scanned
            }
            error = nil
            if selectedID == nil || !filteredDevices.contains(where: { $0.id == selectedID }) {
                selectedID = filteredDevices.first?.id
            }
            if autoConnect, !didAutoConnect, transport != "airplay", selected != nil {
                didAutoConnect = true
                connect()
            }
        } catch {
            if !devices.isEmpty {
                devices = []
            }
            if transport != "airplay" {
                self.error = "Couldn’t scan for iPhones. Check that Apple Mobile Device services are running."
            }
        }
    }

    func pairWifi() async {
        guard !isPairing else { return }
        guard let usb = usbDevice else {
            self.error = "Connect your iPhone via USB cable first, then click Pair Wi-Fi."
            return
        }
        isPairing = true
        error = nil
        pairingStatus = "Pairing with \(usb.displayName) over USB…"
        defer { isPairing = false }

        let executableURL = backendURL
        let udid = usb.udid
        do {
            try await Task.detached(priority: .userInitiated) {
                let process = Process()
                let errPipe = Pipe()
                process.executableURL = executableURL
                process.arguments = ["pair", "--udid", udid]
                process.standardOutput = Pipe()
                process.standardError = errPipe
                try process.run()
                let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
                process.waitUntilExit()
                guard process.terminationStatus == 0 else {
                    let msg = String(data: errData, encoding: .utf8)?
                        .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                    throw NSError(
                        domain: "iPhoneMirrorPair",
                        code: Int(process.terminationStatus),
                        userInfo: [NSLocalizedDescriptionKey: msg.isEmpty ? "Pairing failed. Unlock iPhone and tap Trust if prompted." : msg]
                    )
                }
            }.value
            pairingStatus = "Wi-Fi paired! Direct Wi-Fi mirroring is ready."
            await scan(manual: false)
        } catch {
            pairingStatus = nil
            self.error = error.localizedDescription
        }
    }

    func connect() {
        guard backend == nil else { return }
        if transport != "airplay", selected == nil {
            return
        }

        let process = Process()
        process.executableURL = backendURL
        process.arguments = ["mirror"]
        var environment = ProcessInfo.processInfo.environment
        environment["IPHONE_MIRROR_SESSION_WORKER"] = "1"
        environment["IPHONE_MIRROR_AUTO_ROTATE"] = autoRotate ? "1" : "0"
        if let pending = pendingLaunchBundleID, !pending.isEmpty {
            environment["IPHONE_MIRROR_LAUNCH_BUNDLE_ID"] = pending
            pendingLaunchBundleID = nil
        }

        if transport == "airplay" {
            environment["IPHONE_MIRROR_TRANSPORT"] = "airplay"
            if let device = selected {
                environment["IPHONE_MIRROR_UDID"] = device.udid
                if device.isRemote, let host = device.host, let port = device.port {
                    environment["IPHONE_MIRROR_WIFI_HOST"] = host
                    environment["IPHONE_MIRROR_WIFI_PORT"] = String(port)
                    environment["IPHONE_MIRROR_WIFI_NAME"] = device.name ?? device.udid
                    environment["IPHONE_MIRROR_WIFI_IDENTIFIER"] = device.udid
                }
            }
        } else if let device = selected {
            environment["IPHONE_MIRROR_UDID"] = device.udid
            environment["IPHONE_MIRROR_TRANSPORT"] = device.connection_type == .network ? "wifi" : "usb"
            if device.isRemote, let host = device.host, let port = device.port {
                environment["IPHONE_MIRROR_WIFI_HOST"] = host
                environment["IPHONE_MIRROR_WIFI_PORT"] = String(port)
                environment["IPHONE_MIRROR_WIFI_NAME"] = device.name ?? device.udid
                environment["IPHONE_MIRROR_WIFI_IDENTIFIER"] = device.udid
            } else {
                environment.removeValue(forKey: "IPHONE_MIRROR_WIFI_HOST")
                environment.removeValue(forKey: "IPHONE_MIRROR_WIFI_PORT")
                environment.removeValue(forKey: "IPHONE_MIRROR_WIFI_NAME")
                environment.removeValue(forKey: "IPHONE_MIRROR_WIFI_IDENTIFIER")
            }
        }

        process.environment = environment
        let logURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("iphone-mirror-backend.log")
        FileManager.default.createFile(atPath: logURL.path, contents: nil)
        if let handle = try? FileHandle(forWritingTo: logURL) {
            process.standardOutput = handle
            process.standardError = handle
        }
        process.terminationHandler = { [weak self] process in
            Task { @MainActor in
                self?.backend = nil
                self?.backendRunning = false
                self?.daemonBridge?.sendSetMirrorActive(false)
                self?.menuBarController.rebuildMenu()
                Self.showLauncher()
                if process.terminationStatus != 0 {
                    let detail = (try? String(contentsOf: logURL, encoding: .utf8))?
                        .split(separator: "\n")
                        .suffix(2)
                        .joined(separator: " ")
                    self?.error = detail?.isEmpty == false
                        ? detail!
                        : "The mirror session ended. Keep the iPhone unlocked, trusted, and on the same network, then reconnect."
                }
            }
        }
        do {
            try process.run()
            backend = process
            backendRunning = true
            daemonBridge?.sendSetMirrorActive(true)
            menuBarController.rebuildMenu()
            error = nil
            Self.hideLauncher()
            NSRunningApplication(processIdentifier: process.processIdentifier)?
                .activate(options: [.activateAllWindows, .activateIgnoringOtherApps])
        } catch {
            Self.showLauncher()
            self.error = "Couldn’t start the mirror session."
        }
    }

    func disconnect() {
        backend?.terminate()
    }

    private static func hideLauncher() {
        NSApp.setActivationPolicy(.accessory)
        NSApp.hide(nil)
    }

    private static func showLauncher() {
        NSApp.setActivationPolicy(.regular)
        NSApp.unhide(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

// MARK: - Glassmorphism Primitives (macOS 12 Monterey Compatible)

struct VisualEffectBlur: NSViewRepresentable {
    var material: NSVisualEffectView.Material
    var blendingMode: NSVisualEffectView.BlendingMode = .behindWindow
    var state: NSVisualEffectView.State = .active

    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = material
        view.blendingMode = blendingMode
        view.state = state
        return view
    }

    func updateNSView(_ view: NSVisualEffectView, context: Context) {
        view.material = material
        view.blendingMode = blendingMode
        view.state = state
    }
}

struct WindowGlassConfigurator: NSViewRepresentable {
    var keepInMenuBar: Bool
    var explicitQuit: Bool

    final class Coordinator: NSObject, NSWindowDelegate {
        var keepInMenuBar: Bool
        var explicitQuit: Bool

        init(keepInMenuBar: Bool, explicitQuit: Bool) {
            self.keepInMenuBar = keepInMenuBar
            self.explicitQuit = explicitQuit
        }

        func windowShouldClose(_ sender: NSWindow) -> Bool {
            if keepInMenuBar && !explicitQuit {
                sender.orderOut(nil)
                NSApp.setActivationPolicy(.accessory)
                return false
            }
            return true
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(keepInMenuBar: keepInMenuBar, explicitQuit: explicitQuit)
    }

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.styleMask.insert(.fullSizeContentView)
            window.isMovableByWindowBackground = false
            window.isOpaque = false
            window.backgroundColor = .clear
            window.delegate = context.coordinator
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        context.coordinator.keepInMenuBar = keepInMenuBar
        context.coordinator.explicitQuit = explicitQuit
    }
}

struct GlassSurface<Content: View>: View {
    var cornerRadius: CGFloat = 18
    var material: NSVisualEffectView.Material = .hudWindow
    var tintOpacity: Double = 0.08
    @ViewBuilder var content: () -> Content

    var body: some View {
        ZStack {
            VisualEffectBlur(material: material, blendingMode: .withinWindow)
            RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                .fill(
                    LinearGradient(
                        colors: [
                            Color.white.opacity(tintOpacity + 0.06),
                            Color.white.opacity(tintOpacity * 0.4),
                        ],
                        startPoint: .topLeading,
                        endPoint: .bottomTrailing
                    )
                )
            content()
        }
        .clipShape(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: cornerRadius, style: .continuous)
                .strokeBorder(
                    LinearGradient(
                        colors: [
                            Color.white.opacity(0.38),
                            Color.white.opacity(0.10),
                            Color.white.opacity(0.22),
                        ],
                        startPoint: .topLeading,
                        endPoint: .bottomTrailing
                    ),
                    lineWidth: 1
                )
        )
        .shadow(color: Color.black.opacity(0.28), radius: 18, x: 0, y: 8)
    }
}

struct AmbientGlassBackdrop: View {
    var accent: Color

    var body: some View {
        ZStack {
            VisualEffectBlur(material: .underWindowBackground, blendingMode: .behindWindow)
            Color(red: 0.085, green: 0.09, blue: 0.105).opacity(0.92)
        }
        .ignoresSafeArea()
    }
}

struct DeviceRow: View {
    let device: Device
    let selected: Bool

    var body: some View {
        HStack(spacing: 12) {
            ZStack {
                Circle()
                    .fill(
                        selected
                            ? Color.white.opacity(0.22)
                            : Color.white.opacity(0.08)
                    )
                    .frame(width: 32, height: 32)
                Image(systemName: device.connection_type == .usb ? "cable.connector" : "wifi")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundColor(selected ? .white : Color(red: 0.45, green: 0.80, blue: 1.0))
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(device.displayName)
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundColor(.white)
                    .lineLimit(1)
                Text(device.connection_type.label)
                    .font(.system(size: 11))
                    .foregroundColor(selected ? Color.white.opacity(0.82) : Color.white.opacity(0.55))
            }
            Spacer()
            Circle()
                .fill(selected ? Color.white : Color.green)
                .frame(width: 7, height: 7)
                .shadow(color: Color.green.opacity(selected ? 0 : 0.7), radius: 4)
        }
        .padding(.horizontal, 12)
        .frame(height: 54)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(
                    selected
                        ? LinearGradient(
                            colors: [
                                Color(red: 0.25, green: 0.55, blue: 1.0).opacity(0.85),
                                Color(red: 0.18, green: 0.42, blue: 0.92).opacity(0.75),
                            ],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        )
                        : LinearGradient(
                            colors: [Color.white.opacity(0.06), Color.white.opacity(0.02)],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        )
                )
        )
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .strokeBorder(Color.white.opacity(selected ? 0.35 : 0.10), lineWidth: 1)
        )
        .contentShape(Rectangle())
    }
}

struct GlassModeSelector: View {
    @Binding var selection: String
    var onSelect: () -> Void

    var body: some View {
        HStack(spacing: 4) {
            ForEach(TransportMode.allCases) { mode in
                let isSelected = selection == mode.rawValue
                Button {
                    withAnimation(.easeInOut(duration: 0.18)) {
                        selection = mode.rawValue
                    }
                    onSelect()
                } label: {
                    VStack(spacing: 3) {
                        Image(systemName: mode.icon)
                            .font(.system(size: 11, weight: .semibold))
                        Text(mode.title)
                            .font(.system(size: 10.5, weight: isSelected ? .semibold : .medium))
                    }
                    .foregroundColor(isSelected ? .white : Color.white.opacity(0.65))
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 7)
                    .background(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .fill(
                                isSelected
                                    ? LinearGradient(
                                        colors: [
                                            Color.white.opacity(0.24),
                                            Color.white.opacity(0.10),
                                        ],
                                        startPoint: .top,
                                        endPoint: .bottom
                                    )
                                    : LinearGradient(colors: [.clear, .clear], startPoint: .top, endPoint: .bottom)
                            )
                    )
                    .overlay(
                        RoundedRectangle(cornerRadius: 9, style: .continuous)
                            .strokeBorder(Color.white.opacity(isSelected ? 0.32 : 0.0), lineWidth: 1)
                    )
                    .contentShape(Rectangle())
                }
                .contentShape(Rectangle())
                .buttonStyle(.plain)
            }
        }
        .padding(4)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(Color.black.opacity(0.25))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .strokeBorder(Color.white.opacity(0.12), lineWidth: 1)
        )
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: LauncherModel

    private var stageAccent: Color {
        model.transport == "airplay"
            ? Color(red: 0.42, green: 0.72, blue: 1.0)
            : Color(red: 0.25, green: 0.62, blue: 1.0)
    }

    var body: some View {
        ZStack {
            AmbientGlassBackdrop(accent: stageAccent)
            WindowGlassConfigurator(
                keepInMenuBar: model.keepInMenuBar,
                explicitQuit: model.explicitQuit
            )

            HStack(spacing: 16) {
                sidebarCard
                    .frame(width: 268)

                if model.showNotificationsPanel {
                    NotificationsStageView()
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    stageCard
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .padding(.top, 34)
            .padding([.horizontal, .bottom], 18)
        }
        .preferredColorScheme(.dark)
        .frame(minWidth: 780, idealWidth: 860, minHeight: 520, idealHeight: 560)
        .task { model.start() }
    }

    private var sidebarCard: some View {
        GlassSurface(cornerRadius: 20, material: .hudWindow, tintOpacity: 0.06) {
            VStack(alignment: .leading, spacing: 14) {
                HStack(spacing: 8) {
                    Image(systemName: "iphone.gen3")
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundColor(.white)
                    Text("iPhone Mirror")
                        .font(.system(size: 14, weight: .bold))
                        .foregroundColor(.white)
                    Spacer()
                    Button {
                        withAnimation(.easeInOut(duration: 0.16)) {
                            model.showNotificationsPanel.toggle()
                            if model.showNotificationsPanel {
                                model.unreadNotificationCount = 0
                            }
                        }
                    } label: {
                        ZStack(alignment: .topTrailing) {
                            Image(systemName: model.bleAncsState.isHealthy ? "bell.badge.fill" : "bell")
                                .font(.system(size: 12, weight: .semibold))
                                .foregroundColor(model.showNotificationsPanel ? .white : Color.white.opacity(0.8))
                                .padding(6)
                                .background(
                                    (model.showNotificationsPanel
                                        ? Color(red: 0.28, green: 0.62, blue: 1.0).opacity(0.45)
                                        : Color.white.opacity(0.08)),
                                    in: Circle()
                                )
                            if model.unreadNotificationCount > 0 && !model.showNotificationsPanel {
                                Circle()
                                    .fill(Color.blue)
                                    .frame(width: 7, height: 7)
                                    .offset(x: -1, y: 1)
                            }
                        }
                    }
                    .buttonStyle(.plain)
                    .help("iPhone Notifications")
                    Button { Task { await model.scan(manual: true) } } label: {
                        Image(systemName: "arrow.clockwise")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundColor(model.isManualScanning ? .white : Color.white.opacity(0.8))
                            .rotationEffect(.degrees(model.isManualScanning ? 360 : 0))
                            .animation(
                                model.isManualScanning
                                    ? .linear(duration: 0.7).repeatForever(autoreverses: false)
                                    : .easeOut(duration: 0.2),
                                value: model.isManualScanning
                            )
                            .padding(6)
                            .background(
                                (model.isManualScanning
                                    ? Color(red: 0.28, green: 0.62, blue: 1.0).opacity(0.45)
                                    : Color.white.opacity(0.08)),
                                in: Circle()
                            )
                    }
                    .buttonStyle(.plain)
                    .disabled(model.isManualScanning)
                    .keyboardShortcut("r", modifiers: .command)
                    .help("Scan again")
                }

                GlassModeSelector(selection: $model.transport) {
                    model.selectedID = model.filteredDevices.first?.id
                }

                if model.transport == "airplay" {
                    HStack(spacing: 8) {
                        Image(systemName: "airplayvideo")
                            .foregroundColor(Color(red: 0.55, green: 0.85, blue: 1.0))
                        VStack(alignment: .leading, spacing: 2) {
                            Text("AirPlay Receiver Mode")
                                .font(.system(size: 12, weight: .semibold))
                                .foregroundColor(.white)
                            Text(model.selected != nil
                                ? "Hybrid HID ready via \(model.selected!.displayName)"
                                : "Standard H.264 wireless receiver")
                                .font(.system(size: 10.5))
                                .foregroundColor(Color.white.opacity(0.62))
                                .lineLimit(2)
                        }
                    }
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Color.white.opacity(0.07), in: RoundedRectangle(cornerRadius: 11, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 11, style: .continuous)
                            .strokeBorder(Color.white.opacity(0.16), lineWidth: 1)
                    )
                }

                if model.filteredDevices.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Image(systemName: model.transport == "airplay" ? "antenna.radiowaves.left.and.right" : "iphone.slash")
                            .font(.system(size: 22, weight: .light))
                            .foregroundColor(Color.white.opacity(0.65))
                        Text(model.transport == "airplay" ? "No wired/paired iPhone required" : "No iPhone found")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundColor(.white)
                        Text(
                            model.transport == "airplay"
                                ? "Start the receiver directly, then pick “iPhone Mirror” in iPhone Control Center."
                                : (model.transport == "wifi"
                                    ? "Connect once via cable and trust this Mac to mirror over Wi-Fi."
                                    : "Connect by USB, or pair once by cable for Wi-Fi mirroring.")
                        )
                        .font(.system(size: 11.5))
                        .foregroundColor(Color.white.opacity(0.62))
                        .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.top, 6)
                } else {
                    ScrollView {
                        LazyVStack(spacing: 6) {
                            ForEach(model.filteredDevices) { device in
                                DeviceRow(device: device, selected: model.selected?.id == device.id)
                                    .onTapGesture { model.selectedID = device.id }
                            }
                        }
                    }
                }

                if model.filteredDevices.isEmpty, model.transport != "usb", !model.uniqueNearbyDevices.isEmpty {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Discovered on Wi-Fi")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundColor(Color.white.opacity(0.55))
                        ForEach(model.uniqueNearbyDevices.prefix(3)) { device in
                            HStack(spacing: 8) {
                                Image(systemName: "wifi")
                                    .font(.system(size: 11))
                                    .foregroundColor(Color(red: 0.45, green: 0.80, blue: 1.0))
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(device.displayName)
                                        .font(.system(size: 11.5, weight: .medium))
                                        .foregroundColor(.white)
                                        .lineLimit(1)
                                    Text("Bonjour active")
                                        .font(.system(size: 9.5))
                                        .foregroundColor(Color.white.opacity(0.5))
                                }
                            }
                            .padding(.vertical, 2)
                        }
                    }
                }

                Spacer()

                HStack {
                    VStack(alignment: .leading, spacing: 3) {
                        HStack(spacing: 6) {
                            if model.isManualScanning {
                                ProgressView()
                                    .controlSize(.small)
                                    .scaleEffect(0.58)
                                    .frame(width: 10, height: 10)
                                Text("Scanning…")
                            } else {
                                Circle()
                                    .fill(model.error == nil ? Color.green : Color.orange)
                                    .frame(width: 6, height: 6)
                                Text(model.transport == "airplay" ? "AirPlay ready" : "Discovery active")
                            }
                        }
                        Text(AppInfo.label)
                            .foregroundColor(Color.white.opacity(0.42))
                    }
                    .font(.system(size: 10.5))
                    .foregroundColor(Color.white.opacity(0.65))
                    Spacer()
                    Button {
                        if model.availableUpdate != nil {
                            model.openUpdateDownload()
                        } else {
                            Task { await model.checkForUpdates(silent: false) }
                        }
                    } label: {
                        HStack(spacing: 4) {
                            Image(systemName: model.availableUpdate != nil ? "arrow.down.circle.fill" : "arrow.triangle.2.circlepath")
                                .font(.system(size: 10.5, weight: .semibold))
                            if let update = model.availableUpdate {
                                Text("v\(update.version)")
                                    .font(.system(size: 10, weight: .semibold))
                            }
                        }
                        .foregroundColor(model.availableUpdate != nil ? .white : Color.white.opacity(0.72))
                        .padding(.horizontal, 8)
                        .padding(.vertical, 5)
                        .background(
                            (model.availableUpdate != nil
                                ? Color(red: 0.22, green: 0.56, blue: 1.0).opacity(0.65)
                                : Color.white.opacity(0.08)),
                            in: Capsule()
                        )
                        .overlay(
                            Capsule()
                                .strokeBorder(Color.white.opacity(0.18), lineWidth: 1)
                        )
                    }
                    .buttonStyle(.plain)
                    .disabled(model.isCheckingUpdates)
                    .help(model.availableUpdate != nil ? "Download update" : "Check for GitHub updates")
                }
            }
            .padding(18)
        }
    }

    private var stageCard: some View {
        GlassSurface(cornerRadius: 22, material: .hudWindow, tintOpacity: 0.05) {
            VStack(spacing: 0) {
                if let update = model.availableUpdate, !model.updateDismissed {
                    updateBanner(update)
                }

                Spacer()

                if model.transport == "airplay" {
                    ZStack {
                        Circle()
                            .fill(
                                RadialGradient(
                                    colors: [Color.white.opacity(0.10), .clear],
                                    center: .center,
                                    startRadius: 8,
                                    endRadius: 68
                                )
                            )
                            .frame(width: 136, height: 136)
                        Image(systemName: "airplayvideo")
                            .font(.system(size: 62, weight: .light))
                            .foregroundColor(.white)
                    }
                    .padding(.bottom, 12)

                    Text("AirPlay H.264 Receiver")
                        .font(.system(size: 25, weight: .bold))
                        .foregroundColor(.white)

                    Text("Open Control Center → Screen Mirroring on your iPhone and select “iPhone Mirror”.")
                        .font(.system(size: 13))
                        .foregroundColor(Color.white.opacity(0.75))
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 380)
                        .padding(.top, 8)

                    HStack(spacing: 8) {
                        Image(systemName: model.selected != nil ? "hand.tap.fill" : "eye")
                            .font(.system(size: 11))
                        Text(
                            model.selected != nil
                                ? "Hybrid touch & keyboard enabled via \(model.selected!.connection_type.label)"
                                : "Video + Audio mode (connect USB/Wi-Fi once to add touch control)"
                        )
                        .font(.system(size: 11.5, weight: .medium))
                    }
                    .foregroundColor(Color.white.opacity(0.85))
                    .padding(.horizontal, 14)
                    .padding(.vertical, 7)
                    .background(Color.white.opacity(0.09), in: Capsule())
                    .overlay(Capsule().strokeBorder(Color.white.opacity(0.22), lineWidth: 1))
                    .padding(.top, 16)

                    glassPrimaryButton(
                        title: model.backendRunning ? "Stop Receiver" : "Start AirPlay Receiver"
                    ) {
                        model.backendRunning ? model.disconnect() : model.connect()
                    }
                    .padding(.top, 24)
                } else if let device = model.selected {
                    ZStack {
                        Circle()
                            .fill(
                                RadialGradient(
                                    colors: [Color.white.opacity(0.10), .clear],
                                    center: .center,
                                    startRadius: 8,
                                    endRadius: 72
                                )
                            )
                            .frame(width: 140, height: 140)
                        Image(systemName: device.connection_type == .usb ? "iphone.gen3" : "iphone.radiowaves.left.and.right")
                            .font(.system(size: 66, weight: .ultraLight))
                            .foregroundColor(.white)
                    }
                    .padding(.bottom, 14)

                    Text("Ready to mirror")
                        .font(.system(size: 25, weight: .bold))
                        .foregroundColor(.white)

                    Text("\(device.displayName) · \(device.connection_type.label) HEVC")
                        .font(.system(size: 13))
                        .foregroundColor(Color.white.opacity(0.72))
                        .padding(.top, 6)

                    glassPrimaryButton(
                        title: model.backendRunning ? "Disconnect" : "Start Mirroring"
                    ) {
                        model.backendRunning ? model.disconnect() : model.connect()
                    }
                    .padding(.top, 26)

                    if !model.backendRunning, (model.usbDevice != nil || model.transport == "wifi") {
                        glassPairButton
                            .padding(.top, 10)
                    }
                } else {
                    Image(systemName: "rectangle.on.rectangle.slash")
                        .font(.system(size: 56, weight: .ultraLight))
                        .foregroundColor(Color.white.opacity(0.55))
                        .padding(.bottom, 18)

                    Text("Connect an iPhone")
                        .font(.system(size: 24, weight: .bold))
                        .foregroundColor(.white)

                    Text("Plug in USB for the lowest latency, use paired Wi-Fi, or switch the mode selector to AirPlay.")
                        .font(.system(size: 13))
                        .foregroundColor(Color.white.opacity(0.70))
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 380)
                        .padding(.top, 8)

                    if model.usbDevice != nil || model.transport == "wifi" {
                        glassPairButton
                            .padding(.top, 18)
                    }
                }

                if let status = model.pairingStatus {
                    HStack(spacing: 8) {
                        Image(systemName: model.isPairing ? "arrow.triangle.2.circlepath" : "checkmark.circle.fill")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundColor(Color(red: 0.45, green: 0.90, blue: 0.62))
                        Text(status)
                            .font(.system(size: 12))
                            .foregroundColor(.white)
                    }
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 8)
                    .background(Color.green.opacity(0.14), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .strokeBorder(Color.green.opacity(0.32), lineWidth: 1)
                    )
                    .padding(.top, 16)
                }

                if let error = model.error {
                    Text(error)
                        .font(.system(size: 12))
                        .foregroundColor(Color(red: 1.0, green: 0.55, blue: 0.55))
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 400)
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .background(Color.red.opacity(0.14), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
                        .overlay(
                            RoundedRectangle(cornerRadius: 10, style: .continuous)
                                .strokeBorder(Color.red.opacity(0.32), lineWidth: 1)
                        )
                        .padding(.top, 20)
                }

                Spacer()
            }
            .padding(34)
        }
    }

    private func updateBanner(_ update: AvailableUpdate) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "sparkles")
                .font(.system(size: 12, weight: .semibold))
                .foregroundColor(Color(red: 0.48, green: 0.80, blue: 1.0))
            Text("Update available: v\(update.version)")
                .font(.system(size: 12, weight: .semibold))
                .foregroundColor(.white)
            Spacer()
            Button("Download") {
                model.openUpdateDownload()
            }
            .buttonStyle(.plain)
            .font(.system(size: 11.5, weight: .semibold))
            .foregroundColor(.white)
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(Color(red: 0.25, green: 0.56, blue: 1.0), in: Capsule())
            Button {
                model.updateDismissed = true
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 10, weight: .bold))
                    .foregroundColor(Color.white.opacity(0.6))
                    .padding(4)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .background(Color.white.opacity(0.08), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .strokeBorder(Color.white.opacity(0.20), lineWidth: 1)
        )
        .padding(.bottom, 12)
    }

    private var glassPairButton: some View {
        Button {
            Task { await model.pairWifi() }
        } label: {
            HStack(spacing: 7) {
                if model.isPairing {
                    ProgressView()
                        .controlSize(.small)
                        .scaleEffect(0.7)
                } else {
                    Image(systemName: "link.badge.plus")
                        .font(.system(size: 11.5, weight: .semibold))
                }
                Text(model.isPairing ? "Pairing Wi-Fi…" : "Pair Wi-Fi via USB")
                    .font(.system(size: 12.5, weight: .medium))
            }
            .foregroundColor(model.usbDevice != nil ? .white : Color.white.opacity(0.45))
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .background(Color.white.opacity(0.08), in: Capsule())
            .overlay(
                Capsule()
                    .strokeBorder(Color.white.opacity(0.20), lineWidth: 1)
            )
        }
        .buttonStyle(.plain)
        .disabled(model.isPairing || model.usbDevice == nil)
        .help(
            model.usbDevice != nil
                ? "Pair the USB iPhone for wireless Wi-Fi mirroring"
                : "Connect your iPhone via USB cable to pair Wi-Fi"
        )
    }

    private func glassPrimaryButton(title: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Image(systemName: model.backendRunning ? "stop.fill" : "play.fill")
                    .font(.system(size: 12, weight: .bold))
                Text(title)
                    .font(.system(size: 14, weight: .semibold))
            }
            .foregroundColor(.white)
            .padding(.horizontal, 24)
            .padding(.vertical, 11)
            .background(
                LinearGradient(
                    colors: model.backendRunning
                        ? [Color(red: 0.92, green: 0.30, blue: 0.34), Color(red: 0.78, green: 0.18, blue: 0.24)]
                        : [Color(red: 0.28, green: 0.62, blue: 1.0), Color(red: 0.16, green: 0.44, blue: 0.95)],
                    startPoint: .topLeading,
                    endPoint: .bottomTrailing
                ),
                in: Capsule()
            )
            .overlay(
                Capsule()
                    .strokeBorder(Color.white.opacity(0.38), lineWidth: 1)
            )
            .shadow(color: (model.backendRunning ? Color.red : Color.blue).opacity(0.42), radius: 14, x: 0, y: 6)
        }
        .buttonStyle(.plain)
        .keyboardShortcut(.defaultAction)
    }
}

struct GroupedFormStyle: ViewModifier {
    func body(content: Content) -> some View {
        if #available(macOS 13.0, *) {
            content.formStyle(.grouped)
        } else {
            content.padding(12)
        }
    }
}

struct SettingsView: View {
    @EnvironmentObject private var model: LauncherModel
    @AppStorage("transport") private var transport = "auto"
    @AppStorage("autoRotate") private var autoRotate = true
    @AppStorage("autoConnect") private var autoConnect = false
    @AppStorage("autoCheckUpdates") private var autoCheckUpdates = true

    var body: some View {
        Form {
            Picker("Connection", selection: $transport) {
                Text("Automatic").tag("auto")
                Text("USB only").tag("usb")
                Text("Wi-Fi only").tag("wifi")
                Text("AirPlay (H.264)").tag("airplay")
            }
            Toggle("Connect automatically when an iPhone appears", isOn: $autoConnect)
            Toggle("Resize the mirror window when orientation changes", isOn: $autoRotate)
            Toggle("Check for updates on launch", isOn: $autoCheckUpdates)

            Section("iPhone Notifications (v0.5)") {
                Toggle("Enable iPhone notifications", isOn: $model.notificationsEnabled)
                    .onChange(of: model.notificationsEnabled) { _ in
                        model.handleNotificationsEnabledChanged()
                    }
                Toggle("Keep running in Menu Bar when closed", isOn: $model.keepInMenuBar)
                    .onChange(of: model.keepInMenuBar) { _ in model.syncNotificationSettings() }
                Toggle("Mute banners while mirroring", isOn: $model.muteWhileMirroring)
                    .onChange(of: model.muteWhileMirroring) { _ in model.syncNotificationSettings() }
                Toggle("Hide message previews (Privacy)", isOn: $model.hideNotificationPreviews)
                    .onChange(of: model.hideNotificationPreviews) { _ in model.syncNotificationSettings() }
                Toggle("Enable Local HTTP Relay fallback", isOn: $model.relayEnabled)
                    .onChange(of: model.relayEnabled) { _ in model.syncNotificationSettings() }
                HStack {
                    Text(model.bleAncsState.statusText)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("Send Test") {
                        model.sendTestNotification()
                    }
                }
            }

            HStack(spacing: 10) {
                Button(model.isCheckingUpdates ? "Checking…" : "Check for Updates") {
                    Task { await model.checkForUpdates(silent: false) }
                }
                .disabled(model.isCheckingUpdates)

                if model.availableUpdate != nil {
                    Button("Download Update") {
                        model.openUpdateDownload()
                    }
                }
                Spacer()
                if let msg = model.updateStatusMessage {
                    Text(msg)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }

            HStack {
                Spacer()
                Text(AppInfo.label)
                    .font(.system(size: 11))
                    .foregroundStyle(.tertiary)
                Spacer()
            }
        }
        .modifier(GroupedFormStyle())
        .padding(8)
        .frame(width: 500, height: 470)
    }
}

@main
struct IPhoneMirrorLauncher: App {
    @StateObject private var model = LauncherModel()

    var body: some Scene {
        WindowGroup("iPhone Mirror") {
            ContentView().environmentObject(model)
        }
        .commands {
            CommandGroup(replacing: .appTermination) {
                Button("Quit iPhone Mirror") {
                    model.prepareForExplicitQuit()
                    NSApp.terminate(nil)
                }
                .keyboardShortcut("q", modifiers: .command)
            }
        }

        Settings { SettingsView().environmentObject(model) }
    }
}
