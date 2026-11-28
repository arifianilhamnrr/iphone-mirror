import CoreBluetooth
import Foundation

enum BleAncsState: Equatable {
    case disabled
    case poweredOff
    case unauthorized
    case unsupported
    case scanning
    case connecting(String)
    case connected(String)
    case restricted(String)

    var statusText: String {
        switch self {
        case .disabled:
            return "Notifications disabled"
        case .poweredOff:
            return "Mac Bluetooth is Off"
        case .unauthorized:
            return "Bluetooth permission denied"
        case .unsupported:
            return "BLE unsupported on this Mac"
        case .scanning:
            return "Scanning for paired iPhone (ANCS)…"
        case .connecting(let name):
            return "Connecting to \(name)…"
        case .connected(let name):
            return "ANCS active: \(name)"
        case .restricted(let detail):
            return detail
        }
    }

    var isHealthy: Bool {
        if case .connected = self { return true }
        return false
    }
}

final class CoreBluetoothAncsClient: NSObject, CBCentralManagerDelegate, CBPeripheralDelegate {
    static let ancsServiceUUID = CBUUID(string: "7905F431-B5CE-4E99-A40F-4B1E122D00D0")
    static let notificationSourceUUID = CBUUID(string: "9FBF120D-6301-42D9-8C58-25E699A21DBD")
    static let controlPointUUID = CBUUID(string: "69D1D8F3-45E1-49A8-9821-9BBDFDAAD9D9")
    static let dataSourceUUID = CBUUID(string: "22EAC6E9-24D6-4BB5-BE44-B36ACE7C7BFB")
    static let serviceChangedUUID = CBUUID(string: "2A05")

    var onStateChanged: ((BleAncsState) -> Void)?
    var onConnected: ((String) -> Void)?
    var onDisconnected: ((String) -> Void)?
    var onNotificationSourceData: ((Data) -> Void)?
    var onDataSourceData: ((Data) -> Void)?

    private var central: CBCentralManager?
    private var activePeripheral: CBPeripheral?
    private var notificationSourceChar: CBCharacteristic?
    private var controlPointChar: CBCharacteristic?
    private var dataSourceChar: CBCharacteristic?
    private var discoveryAttempts = 0
    private var shouldRun = false
    private let queue = DispatchQueue(label: "id.web.projectar.iphone-mirror.ancs-ble", qos: .utility)

    func start() {
        shouldRun = true
        if central == nil {
            central = CBCentralManager(delegate: self, queue: queue, options: [
                CBCentralManagerOptionShowPowerAlertKey: false,
            ])
        } else if let central {
            evaluateCentralState(central)
        }
    }

    func stop() {
        shouldRun = false
        queue.async { [weak self] in
            guard let self else { return }
            self.central?.stopScan()
            if let peripheral = self.activePeripheral {
                self.central?.cancelPeripheralConnection(peripheral)
            }
            self.clearCharacteristics()
            self.activePeripheral = nil
            self.emitState(.disabled)
        }
    }

    func reconnect() {
        queue.async { [weak self] in
            guard let self, self.shouldRun, let central = self.central else { return }
            if let peripheral = self.activePeripheral {
                central.cancelPeripheralConnection(peripheral)
            }
            self.clearCharacteristics()
            self.activePeripheral = nil
            self.discoveryAttempts = 0
            self.evaluateCentralState(central)
        }
    }

    func writeControlPoint(_ data: Data) {
        queue.async { [weak self] in
            guard let self,
                  let peripheral = self.activePeripheral,
                  let cp = self.controlPointChar else { return }
            peripheral.writeValue(data, for: cp, type: .withResponse)
        }
    }

    private func clearCharacteristics() {
        notificationSourceChar = nil
        controlPointChar = nil
        dataSourceChar = nil
    }

    private func emitState(_ state: BleAncsState) {
        DispatchQueue.main.async { [weak self] in
            self?.onStateChanged?(state)
        }
    }

    private func evaluateCentralState(_ central: CBCentralManager) {
        guard shouldRun else {
            emitState(.disabled)
            return
        }
        switch central.state {
        case .poweredOn:
            let connected = central.retrieveConnectedPeripherals(withServices: [Self.ancsServiceUUID])
            if let iphone = connected.first {
                connect(to: iphone, using: central)
                return
            }
            emitState(.scanning)
            central.scanForPeripherals(
                withServices: nil,
                options: [
                    CBCentralManagerScanOptionSolicitedServiceUUIDsKey: [Self.ancsServiceUUID],
                    CBCentralManagerScanOptionAllowDuplicatesKey: false,
                ]
            )
        case .poweredOff:
            emitState(.poweredOff)
        case .unauthorized:
            emitState(.unauthorized)
        case .unsupported:
            emitState(.unsupported)
        default:
            emitState(.scanning)
        }
    }

    private func connect(to peripheral: CBPeripheral, using central: CBCentralManager) {
        central.stopScan()
        activePeripheral = peripheral
        peripheral.delegate = self
        discoveryAttempts = 0
        let name = peripheral.name ?? "iPhone"
        emitState(.connecting(name))
        central.connect(peripheral, options: [
            CBConnectPeripheralOptionNotifyOnNotificationKey: true,
        ])
    }

    // MARK: - CBCentralManagerDelegate

    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        evaluateCentralState(central)
    }

    func centralManager(
        _ central: CBCentralManager,
        didDiscover peripheral: CBPeripheral,
        advertisementData: [String: Any],
        rssi RSSI: NSNumber
    ) {
        guard shouldRun else { return }
        let solicited = advertisementData[CBAdvertisementDataSolicitedServiceUUIDsKey] as? [CBUUID] ?? []
        let advertised = advertisementData[CBAdvertisementDataServiceUUIDsKey] as? [CBUUID] ?? []
        let matchesAncs = solicited.contains(Self.ancsServiceUUID) || advertised.contains(Self.ancsServiceUUID)
        if matchesAncs && activePeripheral == nil {
            connect(to: peripheral, using: central)
        }
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        guard shouldRun else { return }
        discoveryAttempts += 1
        peripheral.discoverServices([Self.ancsServiceUUID])
    }

    func centralManager(_ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?) {
        clearCharacteristics()
        activePeripheral = nil
        let msg = error?.localizedDescription ?? "Connection failed"
        emitState(.restricted("BLE connection failed: \(msg)"))
    }

    func centralManager(_ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral, error: Error?) {
        clearCharacteristics()
        activePeripheral = nil
        let reason = error?.localizedDescription ?? "Disconnected"
        DispatchQueue.main.async { [weak self] in
            self?.onDisconnected?(reason)
        }
        if shouldRun {
            queue.asyncAfter(deadline: .now() + 2.0) { [weak self] in
                guard let self, let c = self.central else { return }
                self.evaluateCentralState(c)
            }
        }
    }

    // MARK: - CBPeripheralDelegate

    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        if let error {
            emitState(.restricted("ANCS service discovery error: \(error.localizedDescription)"))
            return
        }
        guard let ancs = peripheral.services?.first(where: { $0.uuid == Self.ancsServiceUUID }) else {
            if discoveryAttempts < 2 {
                discoveryAttempts += 1
                queue.asyncAfter(deadline: .now() + 1.0) {
                    peripheral.discoverServices([Self.ancsServiceUUID])
                }
            } else {
                emitState(.restricted(
                    "ANCS blocked or disabled: toggle 'Share System Notifications' on iPhone Settings > Bluetooth, or use Local Relay fallback."
                ))
            }
            return
        }
        peripheral.discoverCharacteristics(
            [Self.dataSourceUUID, Self.controlPointUUID, Self.notificationSourceUUID],
            for: ancs
        )
    }

    func peripheral(_ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService, error: Error?) {
        if let error {
            emitState(.restricted("ANCS characteristic error: \(error.localizedDescription)"))
            return
        }
        guard let chars = service.characteristics else { return }
        for c in chars {
            if c.uuid == Self.dataSourceUUID { dataSourceChar = c }
            if c.uuid == Self.controlPointUUID { controlPointChar = c }
            if c.uuid == Self.notificationSourceUUID { notificationSourceChar = c }
        }
        // Apple ANCS spec: subscribe Data Source FIRST, then Notification Source after confirmation.
        if let ds = dataSourceChar {
            peripheral.setNotifyValue(true, for: ds)
        } else {
            emitState(.restricted("ANCS Data Source characteristic missing."))
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateNotificationStateFor characteristic: CBCharacteristic, error: Error?) {
        if let error {
            emitState(.restricted("ANCS subscription requires pairing: \(error.localizedDescription)"))
            return
        }
        if characteristic.uuid == Self.dataSourceUUID && characteristic.isNotifying {
            if let ns = notificationSourceChar {
                peripheral.setNotifyValue(true, for: ns)
            }
        } else if characteristic.uuid == Self.notificationSourceUUID && characteristic.isNotifying {
            let name = peripheral.name ?? "iPhone"
            emitState(.connected(name))
            DispatchQueue.main.async { [weak self] in
                self?.onConnected?(name)
            }
        }
    }

    func peripheral(_ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic, error: Error?) {
        guard error == nil, let value = characteristic.value else { return }
        if characteristic.uuid == Self.notificationSourceUUID {
            DispatchQueue.main.async { [weak self] in
                self?.onNotificationSourceData?(value)
            }
        } else if characteristic.uuid == Self.dataSourceUUID {
            DispatchQueue.main.async { [weak self] in
                self?.onDataSourceData?(value)
            }
        }
    }
}
