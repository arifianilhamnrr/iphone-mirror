import AppKit
import Network
import SwiftUI

struct Device: Codable, Identifiable, Hashable {
    let udid: String
    let connection_type: ConnectionType
    let device_id: UInt32
    var id: String { "\(udid)-\(connection_type.id)" }
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

@MainActor
final class LauncherModel: ObservableObject {
    @Published var devices: [Device] = []
    @Published var selectedID: String?
    @Published var isScanning = false
    @Published var nearbyDevices: [NearbyDevice] = []
    @Published var bonjourScanning = false
    @Published var error: String?
    @Published var backendRunning = false

    @AppStorage("transport") var transport = "auto"
    @AppStorage("autoRotate") var autoRotate = true
    @AppStorage("autoConnect") var autoConnect = false

    private var backend: Process?
    private var scanTask: Task<Void, Never>?
    private var didAutoConnect = false
    private var browsers: [NWBrowser] = []
    private var nearbyByService: [String: Set<String>] = [:]

    var backendURL: URL {
        Bundle.main.bundleURL.appendingPathComponent(
            "Contents/Helpers/iPhone Mirror Stream.app/Contents/MacOS/iphone-mirror-backend"
        )
    }

    var filteredDevices: [Device] {
        let matches = devices.filter { device in
            transport == "auto" || (transport == "usb" && device.connection_type == .usb)
                || (transport == "wifi" && device.connection_type == .network)
        }
        var seen = Set<String>()
        return matches.filter { device in
            if transport == "auto", seen.contains(device.udid) { return false }
            seen.insert(device.udid)
            return true
        }
    }

    var selected: Device? {
        filteredDevices.first(where: { $0.id == selectedID }) ?? filteredDevices.first
    }

    func start() {
        guard scanTask == nil else { return }
        startBonjourScanning()
        scanTask = Task {
            while !Task.isCancelled {
                await scan()
                try? await Task.sleep(for: .seconds(2))
            }
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

    func scan() async {
        isScanning = true
        defer { isScanning = false }
        let process = Process()
        let pipe = Pipe()
        process.executableURL = backendURL
        process.arguments = ["devices", "--json"]
        process.standardOutput = pipe
        process.standardError = Pipe()
        do {
            try process.run()
            process.waitUntilExit()
            let data = pipe.fileHandleForReading.readDataToEndOfFile()
            guard process.terminationStatus == 0 else {
                throw NSError(domain: "iPhoneMirror", code: Int(process.terminationStatus), userInfo: nil)
            }
            devices = try JSONDecoder().decode([Device].self, from: data)
                .sorted { lhs, rhs in
                    if lhs.udid == rhs.udid { return lhs.connection_type == .usb }
                    return lhs.udid < rhs.udid
                }
            error = nil
            if selectedID == nil || !filteredDevices.contains(where: { $0.id == selectedID }) {
                selectedID = filteredDevices.first?.id
            }
            if autoConnect, !didAutoConnect, selected != nil {
                didAutoConnect = true
                connect()
            }
        } catch {
            devices = []
            self.error = "Couldn’t scan for iPhones. Check that Apple Mobile Device services are running."
        }
    }

    func connect() {
        guard backend == nil, let device = selected else { return }
        let process = Process()
        process.executableURL = backendURL
        process.arguments = ["mirror"]
        var environment = ProcessInfo.processInfo.environment
        environment["IPHONE_MIRROR_SESSION_WORKER"] = "1"
        environment["IPHONE_MIRROR_UDID"] = device.udid
        environment["IPHONE_MIRROR_TRANSPORT"] = device.connection_type == .network ? "wifi" : "usb"
        environment["IPHONE_MIRROR_AUTO_ROTATE"] = autoRotate ? "1" : "0"
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
            error = nil
            NSRunningApplication(processIdentifier: process.processIdentifier)?
                .activate(options: [.activateAllWindows, .activateIgnoringOtherApps])
        } catch {
            self.error = "Couldn’t start the mirror session."
        }
    }

    func disconnect() {
        backend?.terminate()
    }
}

struct DeviceRow: View {
    let device: Device
    let selected: Bool

    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: device.connection_type == .usb ? "cable.connector" : "wifi")
                .font(.system(size: 17, weight: .medium))
                .frame(width: 28, height: 28)
                .foregroundStyle(selected ? Color.white : Color.accentColor)
            VStack(alignment: .leading, spacing: 3) {
                Text("iPhone")
                    .font(.system(size: 14, weight: .semibold))
                Text(device.connection_type.label)
                    .font(.system(size: 12))
                    .foregroundStyle(selected ? Color.white.opacity(0.78) : Color.secondary)
            }
            Spacer()
            Circle()
                .fill(selected ? Color.white.opacity(0.8) : Color.green)
                .frame(width: 7, height: 7)
        }
        .padding(.horizontal, 12)
        .frame(height: 58)
        .background(selected ? Color.accentColor : Color.clear, in: RoundedRectangle(cornerRadius: 10))
        .contentShape(Rectangle())
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: LauncherModel

    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 18) {
                HStack {
                    Text("Devices")
                        .font(.system(size: 13, weight: .semibold))
                    Spacer()
                    if model.isScanning { ProgressView().controlSize(.small) }
                    Button { Task { await model.scan() } } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .buttonStyle(.plain)
                    .keyboardShortcut("r", modifiers: .command)
                    .help("Scan again")
                }

                Picker("Connection", selection: $model.transport) {
                    Text("Auto").tag("auto")
                    Text("USB").tag("usb")
                    Text("Wi-Fi").tag("wifi")
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                .onChange(of: model.transport) { _ in
                    model.selectedID = model.filteredDevices.first?.id
                    Task { await model.scan() }
                }

                if model.filteredDevices.isEmpty {
                    VStack(alignment: .leading, spacing: 9) {
                        Image(systemName: "iphone.slash")
                            .font(.system(size: 24, weight: .light))
                            .foregroundStyle(.secondary)
                        Text("No iPhone found")
                            .font(.system(size: 14, weight: .semibold))
                        Text(model.transport == "wifi"
                            ? "Pair once by cable, then enable “Show this iPhone when on Wi-Fi” in Finder."
                            : "Connect by USB, or choose Wi-Fi after pairing in Finder.")
                            .font(.system(size: 12))
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.top, 12)
                } else {
                    ScrollView {
                        LazyVStack(spacing: 4) {
                            ForEach(model.filteredDevices) { device in
                                DeviceRow(device: device, selected: model.selected?.id == device.id)
                                    .onTapGesture { model.selectedID = device.id }
                            }
                        }
                    }
                }

                if model.transport != "usb", !model.nearbyDevices.isEmpty {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Nearby on Wi-Fi")
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(.secondary)
                        ForEach(model.nearbyDevices) { device in
                            HStack(spacing: 10) {
                                Image(systemName: "wifi")
                                    .foregroundStyle(.secondary)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(device.displayName)
                                        .font(.system(size: 12, weight: .medium))
                                        .lineLimit(1)
                                    Text("Pair in Finder to enable mirroring")
                                        .font(.system(size: 10))
                                        .foregroundStyle(.tertiary)
                                }
                            }
                            .padding(.vertical, 4)
                        }
                    }
                }
                Spacer()
                HStack(spacing: 6) {
                    Circle()
                        .fill(model.error == nil ? Color.green : Color.orange)
                        .frame(width: 6, height: 6)
                    Text(model.isScanning ? "Scanning devices…" : "USB + Wi-Fi discovery active")
                }
                    .font(.system(size: 11))
                    .foregroundStyle(.tertiary)
            }
            .padding(20)
            .frame(width: 238)
            .background(Color(nsColor: .windowBackgroundColor))

            Rectangle().fill(Color(nsColor: .separatorColor)).frame(width: 1)

            VStack(spacing: 0) {
                Spacer()
                if let device = model.selected {
                    Image(systemName: device.connection_type == .usb ? "iphone.gen3" : "iphone.radiowaves.left.and.right")
                        .font(.system(size: 70, weight: .ultraLight))
                        .foregroundStyle(Color.accentColor)
                        .padding(.bottom, 24)
                    Text("Ready to mirror")
                        .font(.system(size: 25, weight: .semibold))
                    Text("Connected over \(device.connection_type.label)")
                        .foregroundStyle(.secondary)
                        .padding(.top, 7)
                    Button(model.backendRunning ? "Disconnect" : "Start Mirroring") {
                        model.backendRunning ? model.disconnect() : model.connect()
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .keyboardShortcut(.defaultAction)
                    .padding(.top, 28)
                } else {
                    Image(systemName: "rectangle.on.rectangle.slash")
                        .font(.system(size: 60, weight: .ultraLight))
                        .foregroundStyle(.secondary)
                        .padding(.bottom, 24)
                    Text("Connect an iPhone")
                        .font(.system(size: 25, weight: .semibold))
                    Text("Use USB for the lowest latency, or pair once in Finder to mirror over Wi-Fi.")
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 370)
                        .padding(.top, 8)
                }
                if let error = model.error {
                    Text(error)
                        .font(.system(size: 12))
                        .foregroundStyle(.red)
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 390)
                        .padding(.top, 22)
                }
                Spacer()
            }
            .frame(maxWidth: .infinity)
            .padding(36)
            .background(Color(nsColor: .controlBackgroundColor))
        }
        .frame(minWidth: 720, minHeight: 480)
        .task { model.start() }
    }
}

struct SettingsView: View {
    @AppStorage("transport") private var transport = "auto"
    @AppStorage("autoRotate") private var autoRotate = true
    @AppStorage("autoConnect") private var autoConnect = false

    var body: some View {
        Form {
            Picker("Connection", selection: $transport) {
                Text("Automatic").tag("auto")
                Text("USB only").tag("usb")
                Text("Wi-Fi only").tag("wifi")
            }
            Toggle("Connect automatically when an iPhone appears", isOn: $autoConnect)
            Toggle("Resize the mirror window when orientation changes", isOn: $autoRotate)
        }
        .formStyle(.grouped)
        .padding(8)
        .frame(width: 460, height: 230)
    }
}

@main
struct IPhoneMirrorLauncher: App {
    @StateObject private var model = LauncherModel()

    var body: some Scene {
        WindowGroup("iPhone Mirror") {
            ContentView().environmentObject(model)
        }
        .defaultSize(width: 820, height: 540)
        .windowResizability(.contentMinSize)

        Settings { SettingsView() }
    }
}
