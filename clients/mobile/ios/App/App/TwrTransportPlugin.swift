import Foundation
import Security
import Capacitor
import AVFoundation
import UIKit

private final class SocketSendCompletion {
    private let lock=NSLock()
    private var finished=false
    private let call: CAPPluginCall
    init(_ call: CAPPluginCall) { self.call=call }
    func complete(_ error: String?) {
        lock.lock();guard !finished else {lock.unlock();return};finished=true;lock.unlock()
        if let error {call.reject(error)} else {call.resolve()}
    }
}

/// A pinned `connect` call: settled once, by the first `authenticated` message or the end.
private final class PinnedConnectAttempt {
    private let lock = NSLock()
    private var settled = false
    private let call: CAPPluginCall
    /// Touched only on the socket's queue.
    var authenticated = false
    init(_ call: CAPPluginCall) { self.call = call }
    func resolve() { if settle() { call.resolve() } }
    func reject(_ message: String, _ code: String? = nil) { if settle() { call.reject(message, code) } }
    private func settle() -> Bool { lock.lock(); defer { lock.unlock() }; if settled { return false }; settled = true; return true }
}

/// Hubs come in two kinds that never mix. Certificate-mode hubs (a saved `url`, no `pin`) use
/// URLSession with system trust. Hubs paired by pin (`pin`, `routes`, `last`) use
/// Network.framework with the pin check on every connection (TwrPinnedTransport.swift).
@objc(TwrTransportPlugin)
public final class TwrTransportPlugin: CAPPlugin, CAPBridgedPlugin, URLSessionTaskDelegate {
    public let identifier = "TwrTransportPlugin"
    public let jsName = "TwrTransport"
    public let pluginMethods: [CAPPluginMethod] = ["listHubs", "pair", "connect", "send", "disconnect", "removeHub", "scanInvitation", "updateRoutes"].map { CAPPluginMethod(name: $0, returnType: CAPPluginReturnPromise) }
    private let connectionLock=NSLock()
    /// The one hub socket: a URLSessionWebSocketTask (certificate mode) or a PinnedSocket.
    private var currentSocket: AnyObject?
    /// Bumped by every connect and disconnect, so a pinned connect still racing its routes
    /// knows it was superseded.
    private var connectionGeneration = 0
    private var activeHub: String?
    private let service = "tools.twr.mobile.hubs"
    private let storageLock = NSLock()
    /// Serializes read-modify-write of the vault (pairing, route updates, forgetting).
    private let hubUpdateLock = NSLock()
    private let networkWatch = NetworkWatch()
    private lazy var network: URLSession = {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 40
        return URLSession(configuration: configuration, delegate: self, delegateQueue: nil)
    }()
    private lazy var socketNetwork: URLSession = {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 120
        configuration.timeoutIntervalForResource = 7 * 24 * 60 * 60
        return URLSession(configuration: configuration, delegate: self, delegateQueue: nil)
    }()
    private let sendQueue = DispatchQueue(label: "tools.twr.socket-send")
    public func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }

    override public func load() {
        networkWatch.onChange = { [weak self] type in self?.notifyListeners("network", data: ["type": type]) }
        networkWatch.start()
    }

    private func query() -> [String: Any] { [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: "paired-hubs"] }
    private func vault() throws -> [String: [String: Any]] {
        storageLock.lock(); defer { storageLock.unlock() }
        var q = query(); q[kSecReturnData as String] = true
        var result: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &result)
        if status == errSecItemNotFound { return [:] }
        #if DEBUG
        if status != errSecSuccess { NSLog("twr Keychain read status: %d", status) }
        #endif
        guard status == errSecSuccess, let data = result as? Data, let hubs = try JSONSerialization.jsonObject(with: data) as? [String: [String: Any]] else { throw Failure("Could not read paired hubs from Keychain") }
        return hubs
    }
    private func save(_ hubs: [String: [String: Any]]) throws {
        storageLock.lock(); defer { storageLock.unlock() }
        let data = try JSONSerialization.data(withJSONObject: hubs)
        let status = SecItemUpdate(query() as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var q = query(); q[kSecValueData as String] = data; q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            guard SecItemAdd(q as CFDictionary, nil) == errSecSuccess else { throw Failure("Could not save paired hub in Keychain") }
        } else if status != errSecSuccess { throw Failure("Could not update paired hub in Keychain") }
    }
    private func insertHub(_ id: String, _ record: [String: Any]) throws {
        hubUpdateLock.lock(); defer { hubUpdateLock.unlock() }
        var hubs = try vault(); hubs[id] = record; try save(hubs)
    }
    /// Changes one saved hub in place; false when there is no such hub.
    @discardableResult
    private func updateHub(_ id: String, _ change: (inout [String: Any]) -> Void) throws -> Bool {
        hubUpdateLock.lock(); defer { hubUpdateLock.unlock() }
        var hubs = try vault()
        guard var hub = hubs[id] else { return false }
        change(&hub)
        hubs[id] = hub
        try save(hubs)
        return true
    }
    private func endpoint(_ value: String) throws -> URL {
        guard let url = URL(string: value), let scheme = url.scheme, let host = url.host, url.user == nil, url.password == nil, url.query == nil, url.fragment == nil, url.path.isEmpty || url.path == "/" else { throw Failure("Enter a valid hub URL") }
        var local = false
        #if DEBUG
        local = ["localhost", "127.0.0.1", "::1"].contains(host)
        #endif
        guard scheme == "https" || (scheme == "http" && local) else { throw Failure("Use an HTTPS hub URL") }
        return url
    }
    /// What the web UI sees of a hub: never the token.
    private func metadata(_ id: String, _ hub: [String: Any]) -> [String: Any] {
        var value: [String: Any] = ["id": id, "name": hub["name"] ?? "My hub", "url": hub["url"] ?? "", "scopes": hub["scopes"] ?? [], "gateway_id": hub["gateway_id"] ?? ""]
        if let pin = hub["pin"] as? String {
            value["pin"] = pin
            value["routes"] = RouteBook.decode(hub["routes"]).map(\.route)
            if let route = hub["route"] as? String { value["route"] = route }
        }
        return value
    }
    private struct Failure: LocalizedError {
        let message: String
        let code: String?
        init(_ message: String, code: String? = nil) { self.message = message; self.code = code }
        var errorDescription: String? { message }
    }
    private func reject(_ call: CAPPluginCall, _ error: Error) {
        if let failure = error as? RaceFailure { call.reject(failure.localizedDescription, failure.pinMismatch ? "PIN_MISMATCH" : nil) }
        else if let failure = error as? Failure { call.reject(failure.message, failure.code) }
        else { call.reject(error.localizedDescription) }
    }
    private var invitationScanner: InvitationScanner?
    private var scanningInvitation = false
    @objc public func scanInvitation(_ call: CAPPluginCall) {
        DispatchQueue.main.async { [self] in
            guard !scanningInvitation else { call.reject("The QR scanner is already open", "SCAN_BUSY"); return }
            scanningInvitation = true
            AVCaptureDevice.requestAccess(for: .video) { [self] allowed in
                DispatchQueue.main.async { [self] in
                    guard allowed else { scanningInvitation = false; call.reject("Camera permission was denied. Allow it in Settings or enter the code manually.", "CAMERA_PERMISSION_DENIED"); return }
                    guard let presenter = self.bridge?.viewController else { scanningInvitation = false; call.reject("The camera is unavailable. Enter the code manually.", "CAMERA_UNAVAILABLE"); return }
                    guard presenter.presentedViewController == nil, presenter.viewIfLoaded?.window != nil else { scanningInvitation = false; call.reject("Another camera or dialog is already open", "SCAN_BUSY"); return }
                    let scanner = InvitationScanner { [weak self] result in
                        self?.invitationScanner = nil; self?.scanningInvitation = false
                        switch result {
                        case .success(let value):
                            if value.utf8.count <= 2048 { call.resolve(["invitation": value]) } else { call.reject("Scan a twr pairing invitation") }
                        case .failure(.cancelled): call.reject("QR scan cancelled", "SCAN_CANCELLED")
                        case .failure(.unavailable): call.reject("This device has no available QR camera. Enter the code manually.", "CAMERA_UNAVAILABLE")
                        }
                    }
                    invitationScanner = scanner; presenter.present(scanner, animated: true)
                }
            }
        }
    }
    @objc public func listHubs(_ call: CAPPluginCall) { do { call.resolve(["hubs": try vault().map { metadata($0.key, $0.value) }]) } catch { call.reject(error.localizedDescription) } }

    // MARK: pairing

    /// `pin`: a scanned version 2 invitation. `address`: typed pairing with the pin proof.
    /// Otherwise `url`: certificate mode, as before.
    @objc public func pair(_ call: CAPPluginCall) {
        if let pin = call.getString("pin"), !pin.isEmpty { pairScanned(call, pinText: pin) }
        else if let address = call.getString("address"), !address.trimmingCharacters(in: .whitespaces).isEmpty { pairTyped(call, address: address) }
        else { pairCertificate(call) }
    }
    private func ensureNotPaired(_ gateway: String) throws {
        guard !gateway.isEmpty, try vault().values.contains(where: { $0["gateway_id"] as? String == gateway }) else { return }
        throw Failure("This hub is already paired. Use Revoke this device in its settings, or Forget this hub under Device access if it cannot connect, then pair again.")
    }
    private func pairCertificate(_ call: CAPPluginCall) {
        Task {
            do {
                let url = try endpoint(call.getString("url") ?? "")
                let (healthData, healthResponse) = try await network.data(from:url.appendingPathComponent("health"))
                guard (healthResponse as? HTTPURLResponse)?.statusCode == 200,
                      let health=try JSONSerialization.jsonObject(with:healthData) as? [String:Any] else {throw Failure("Could not identify the hub")}
                if let gateway=health["gateway_id"] as? String { try ensureNotPaired(gateway) }
                var request = URLRequest(url: url.appendingPathComponent("v1/pair")); request.httpMethod = "POST"; request.setValue("application/json", forHTTPHeaderField: "Content-Type")
                request.httpBody = try JSONSerialization.data(withJSONObject: ["code": call.getString("code") ?? "", "name": call.getString("name") ?? ""])
                let (data, response) = try await network.data(for: request)
                if (response as? HTTPURLResponse)?.statusCode != 200 {
                    let detail=(try? JSONSerialization.jsonObject(with:data) as? [String:Any])?["error"] as? String
                    throw Failure(String((detail ?? "Pairing failed. Check the code and hub URL.").prefix(240)))
                }
                guard (response as? HTTPURLResponse)?.statusCode == 200, var record = try JSONSerialization.jsonObject(with: data) as? [String: Any], let token = record["token"] as? String, token.count == 64 else { throw Failure("Pairing failed. Check the code and hub URL.") }
                // The mode is how it was paired: a field in the answer never makes this a pinned
                // hub, so a credential never crosses modes.
                for pinned in ["pin", "iroh", "routes", "last", "route"] { record.removeValue(forKey: pinned) }
                let id = UUID().uuidString; record["url"] = url.absoluteString; record["name"] = call.getString("hubName") ?? "My hub"
                try insertHub(id, record)
                call.resolve(metadata(id, record))
            } catch { call.reject(error.localizedDescription) }
        }
    }
    /// Races the invitation's routes with its pin, then pairs on the winner.
    private func pairScanned(_ call: CAPPluginCall, pinText: String) {
        Task {
            do {
                let hubName = call.getString("hubName") ?? "My hub"
                guard let pin = Pairing.decodePin(pinText) else { throw Failure("This invitation's hub key is malformed. Scan it again.") }
                guard let code = Pairing.normalizeCode(call.getString("code") ?? "") else { throw Failure("This invitation's code is malformed. Scan it again.") }
                let gatewayID = call.getString("gatewayId") ?? ""
                let routes = (call.getArray("routes", String.self) ?? []).compactMap { PairingRoute($0)?.text }
                try ensureNotPaired(gatewayID)
                let plan = RouteBook.racePlan(routes, remembered: nil, cellular: networkWatch.current == "cellular")
                let found = try await RouteRace.run(plan, pin: pin, gatewayID: gatewayID.isEmpty ? nil : gatewayID, hubName: hubName, timeout: 20)
                try ensureNotPaired(found.gatewayID)
                let record = try await postPair(found.route, pin: pin, code: code, name: call.getString("name") ?? "", hubName: hubName)
                let (id, saved) = try savePinned(record, hubName: hubName, pin: pin, routes: routes, iroh: call.getString("iroh"), winner: found.route, gatewayID: found.gatewayID)
                call.resolve(metadata(id, saved))
            } catch { reject(call, error) }
        }
    }
    /// Typed pairing: capture the presented key, check the hub's proof that it knows the
    /// code for that key, and only then send the code.
    private func pairTyped(_ call: CAPPluginCall, address: String) {
        Task {
            do {
                let hubName = call.getString("hubName") ?? "My hub"
                guard let route = Pairing.typedAddress(address) else { throw Failure("Enter the hub's address as host or host:port") }
                guard let code = Pairing.normalizeCode(call.getString("code") ?? "") else { throw Failure("Enter the 12-character pairing code the hub shows", code: "BAD_CODE") }
                // Argon2id (64 MiB) runs on a background queue while the network steps go on.
                async let stretched = TwrTransportPlugin.stretch(code)
                let capture = PinCheck(expected: nil)
                let found: HubProbe.Found
                do { found = try await HubProbe.health(route, check: capture, expectedGateway: nil, timeout: 30) }
                catch let error as RouteError { throw RaceFailure(failures: [(route: route.text, error: error)], hubName: hubName) }
                guard let presented = capture.presented else { throw RaceFailure(failures: [(route: route.text, error: .unsupportedKey)], hubName: hubName) }
                guard found.advertisedPin != nil else {
                    throw Failure("\(route.text): this hub uses its own certificate. Pair it with its https:// address instead.")
                }
                try ensureNotPaired(found.gatewayID)
                var nonce = [UInt8](repeating: 0, count: Pairing.nonceLength)
                guard SecRandomCopyBytes(kSecRandomDefault, nonce.count, &nonce) == errSecSuccess else { throw Failure("Could not make a pairing nonce") }
                let response: HTTPResponse
                do { response = try await PinnedHTTP.get(route, check: PinCheck(expected: presented), path: "/v1/pair/proof?nonce=\(Pairing.base64URL(Data(nonce)))", timeout: 40) }
                catch let error as RouteError { throw RaceFailure(failures: [(route: route.text, error: error)], hubName: hubName) }
                switch response.status {
                case 200: break
                case 404: throw Failure("\(route.text): no pairing code is open on that hub. Start pairing on the hub again.")
                case 429: throw Failure("\(route.text): too many pairing attempts. Wait a minute and try again.")
                default: throw Failure("\(route.text): the hub refused the proof request (\(response.status))")
                }
                guard let proofText = PinnedHTTP.json(response)?["proof"] as? String, let proof = Pairing.base64URLDecode(proofText), proof.count == 32 else {
                    throw Failure("\(route.text): the hub's answer is not a pairing proof")
                }
                guard let stretchedCode = await stretched else { throw Failure("Could not derive the pairing key") }
                guard Pairing.verifyProof(proof, key: Pairing.proofKey(stretched: stretchedCode), pin: presented, nonce: Data(nonce), gatewayID: found.gatewayID) else {
                    throw Failure("\(route.text): the code doesn't match the hub there. Check the code; if it is right, another device is answering at this address.", code: "BAD_CODE")
                }
                let record = try await postPair(route, pin: presented, code: code, name: call.getString("name") ?? "", hubName: hubName)
                let (id, saved) = try savePinned(record, hubName: hubName, pin: presented, routes: [route.text], iroh: nil, winner: route, gatewayID: found.gatewayID)
                call.resolve(metadata(id, saved))
            } catch { reject(call, error) }
        }
    }
    private static func stretch(_ code: String) async -> Data? {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async { continuation.resume(returning: Pairing.stretch(code: code)) }
        }
    }
    /// `POST /v1/pair` on a connection that presents `pin`.
    private func postPair(_ route: PairingRoute, pin: Data, code: String, name: String, hubName: String) async throws -> [String: Any] {
        let body = try JSONSerialization.data(withJSONObject: ["code": code, "name": name])
        let response: HTTPResponse
        do { response = try await PinnedHTTP.postJSON(route, check: PinCheck(expected: pin), path: "/v1/pair", body: body, timeout: 40) }
        catch let error as RouteError { throw RaceFailure(failures: [(route: route.text, error: error)], hubName: hubName) }
        let json = PinnedHTTP.json(response)
        guard response.status == 200 else {
            let detail = (json?["error"] as? String) ?? "Pairing failed (\(response.status)). Check the code."
            throw Failure("\(route.text): \(detail.prefix(240))")
        }
        guard let record = json, let token = record["token"] as? String, token.count == 64 else { throw Failure("\(route.text): pairing failed. Check the code.") }
        return record
    }
    private func savePinned(_ response: [String: Any], hubName: String, pin: Data, routes: [String], iroh: String?, winner: PairingRoute, gatewayID: String) throws -> (String, [String: Any]) {
        var record = response
        record["name"] = hubName
        record["url"] = "https://\(winner.authority ?? "")"
        record["pin"] = Pairing.base64URL(pin)
        if let iroh, !iroh.isEmpty { record["iroh"] = iroh }
        record["routes"] = RouteBook.encode(RouteBook.merge([], routes + [winner.text], now: Date().timeIntervalSince1970 * 1000))
        record["route"] = winner.text
        record["last"] = [networkWatch.slot: winner.text]
        if (record["gateway_id"] as? String ?? "").isEmpty { record["gateway_id"] = gatewayID }
        let id = UUID().uuidString
        try insertHub(id, record)
        return (id, record)
    }

    /// Merges the routes the gateway's hello reports. Certificate-mode hubs have none to keep.
    @objc public func updateRoutes(_ call: CAPPluginCall) {
        let id = call.getString("id") ?? ""
        let routes = call.getArray("routes", String.self) ?? []
        guard routes.count <= 64 else { call.reject("Too many routes"); return }
        do {
            let found = try updateHub(id) { hub in
                guard hub["pin"] != nil else { return }
                let merged = RouteBook.merge(RouteBook.decode(hub["routes"]), routes, now: Date().timeIntervalSince1970 * 1000)
                hub["routes"] = RouteBook.encode(merged)
                let kept = Set(merged.map(\.route))
                hub["last"] = ((hub["last"] as? [String: String]) ?? [:]).filter { kept.contains($0.value) }
            }
            if found { call.resolve() } else { call.reject("Paired hub not found") }
        } catch { call.reject(error.localizedDescription) }
    }

    // MARK: the socket

    private var socket: AnyObject? {
        connectionLock.lock(); defer { connectionLock.unlock() }; return currentSocket
    }
    private func isCurrent(_ candidate: AnyObject) -> Bool {
        connectionLock.lock(); defer { connectionLock.unlock() }; return currentSocket === candidate
    }
    private func isCurrent(generation: Int) -> Bool {
        connectionLock.lock(); defer { connectionLock.unlock() }; return connectionGeneration == generation
    }
    /// Ends the current socket and any pinned connect still racing; returns the new generation.
    private func supersede() -> Int {
        connectionLock.lock()
        connectionGeneration += 1
        let generation = connectionGeneration
        let previous = currentSocket
        currentSocket = nil
        connectionLock.unlock()
        closeSocket(previous)
        return generation
    }
    /// Makes `candidate` the socket unless a later connect or disconnect came first.
    private func install(_ candidate: AnyObject, generation: Int) -> Bool {
        connectionLock.lock(); defer { connectionLock.unlock() }
        guard connectionGeneration == generation else { return false }
        currentSocket = candidate
        return true
    }
    private func closeSocket(_ candidate: AnyObject?, _ code: URLSessionWebSocketTask.CloseCode = .normalClosure) {
        if let task = candidate as? URLSessionWebSocketTask { task.cancel(with: code, reason: nil) }
        else if let pinned = candidate as? PinnedSocket { pinned.close() }
    }

    @objc public func connect(_ call: CAPPluginCall) {
        do {
            let id = call.getString("id") ?? ""
            guard let hub = try vault()[id], let token = hub["token"] as? String else { throw Failure("Paired hub not found") }
            if hub["pin"] != nil { connectPinned(call, id: id, hub: hub, token: token); return }
            guard let rawURL = hub["url"] as? String else { throw Failure("Paired hub not found") }
            let base = try endpoint(rawURL)
            var components = URLComponents(url: base.appendingPathComponent("v1/socket"), resolvingAgainstBaseURL: false)!
            components.scheme = base.scheme == "https" ? "wss" : "ws"
            let generation = supersede()
            var request=URLRequest(url:components.url!);request.setValue("Bearer \(token)",forHTTPHeaderField:"Authorization");request.setValue("1",forHTTPHeaderField:"X-Twr-Framing")
            let task = socketNetwork.webSocketTask(with: request); task.maximumMessageSize=16*1024*1024
            guard install(task, generation: generation) else { call.reject("Connection was superseded"); return }
            activeHub = id; task.resume()
            Task {
                var authenticated = false
                do {
                    while !Task.isCancelled {
                        let frame = try await task.receive()
                        guard case .string(let text) = frame, text.utf8.count <= 16 * 1024 * 1024, let data = text.data(using: .utf8), let message = try JSONSerialization.jsonObject(with: data) as? [String: Any] else { throw Failure("Invalid hub response") }
                        guard self.isCurrent(task) else { if !authenticated { call.reject("Connection was superseded") }; return }
                        if message["type"] as? String == "authenticated" { authenticated = true; call.resolve() }
                        self.notifyListeners("message", data: ["message": message])
                    }
                } catch {
                    // Only 401 means the gateway no longer knows this token. A 403 and
                    // 5xx are refusals the app retries like any other lost connection.
                    let status = (task.response as? HTTPURLResponse)?.statusCode
                    let revoked = status == 401
                    let refused = !authenticated && status != nil && status != 101
                    let message = revoked ? "AUTH_REVOKED: This device no longer has access to the hub. Forget it and pair again." : refused ? "Gateway refused the connection (\(status!))" : "Remote hub connection closed"
                    if !authenticated { call.reject(message, revoked ? "AUTH_REVOKED" : nil) }
                    if self.isCurrent(task) { self.notifyListeners("message", data: ["message": ["type": "transport-closed", "connected": false, "error": message]]) }
                }
            }
        } catch { call.reject(error.localizedDescription) }
    }
    /// Races the saved routes (the one remembered for this network type first), then opens
    /// the socket on the winner and remembers it.
    private func connectPinned(_ call: CAPPluginCall, id: String, hub: [String: Any], token: String) {
        let generation = supersede()
        activeHub = id
        let hubName = hub["name"] as? String ?? "My hub"
        Task {
            do {
                guard let pinText = hub["pin"] as? String, let pin = Pairing.decodePin(pinText) else {
                    throw Failure("This hub's saved key is unreadable. Forget it and pair again.")
                }
                let slot = networkWatch.slot
                let remembered = (hub["last"] as? [String: String])?[slot]
                let plan = RouteBook.racePlan(RouteBook.decode(hub["routes"]).map(\.route), remembered: remembered, cellular: networkWatch.current == "cellular")
                let found = try await RouteRace.run(plan, pin: pin, gatewayID: hub["gateway_id"] as? String, hubName: hubName, timeout: 12)
                guard isCurrent(generation: generation) else { call.reject("Connection was superseded"); return }
                try? updateHub(id) { saved in
                    let now = Date().timeIntervalSince1970 * 1000
                    saved["routes"] = RouteBook.encode(RouteBook.connected(RouteBook.decode(saved["routes"]), found.route.text, now: now))
                    var last = (saved["last"] as? [String: String]) ?? [:]
                    last[slot] = found.route.text
                    saved["last"] = last
                    saved["route"] = found.route.text
                    saved["url"] = "https://\(found.route.authority ?? "")"
                }
                openPinned(call, route: found.route, pin: pin, token: token, hubName: hubName, generation: generation)
            } catch { reject(call, error) }
        }
    }
    private func openPinned(_ call: CAPPluginCall, route: PairingRoute, pin: Data, token: String, hubName: String, generation: Int) {
        guard let socket = PinnedSocket(route: route, pin: pin, token: token) else { call.reject("This hub's saved credentials are unreadable. Forget it and pair again."); return }
        let attempt = PinnedConnectAttempt(call)
        let via = route.text
        socket.onText = { [weak self] socket, text in
            guard let self, let data = text.data(using: .utf8),
                  let message = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] else { return false }
            guard self.isCurrent(socket) else {
                if !attempt.authenticated { attempt.reject("Connection was superseded") }
                return false
            }
            if message["type"] as? String == "authenticated" { attempt.authenticated = true; attempt.resolve() }
            self.notifyListeners("message", data: ["message": message])
            return true
        }
        socket.onEnd = { [weak self] socket, end in
            // As for certificate mode: only 401 means the gateway no longer knows this token.
            let message: String
            var code: String?
            switch end {
            case .refused(401):
                message = "AUTH_REVOKED: This device no longer has access to the hub. Forget it and pair again."
                code = "AUTH_REVOKED"
            case .refused(let status):
                message = "Gateway refused the connection (\(status)) on \(via)"
            case .pinMismatch:
                message = "PIN_MISMATCH: \(via) is another device, not \(hubName)"
                code = "PIN_MISMATCH"
            case .timedOut:
                message = "Remote hub connection timed out on \(via)"
            case .closed:
                message = "Remote hub connection closed on \(via)"
            }
            if !attempt.authenticated { attempt.reject(message, code) }
            if let self, self.isCurrent(socket) {
                self.notifyListeners("message", data: ["message": ["type": "transport-closed", "connected": false, "error": message]])
            }
        }
        guard install(socket, generation: generation) else { call.reject("Connection was superseded"); return }
        socket.start()
    }
    @objc public func send(_ call: CAPPluginCall) {
        guard let active = socket, let message = call.getString("message") else { call.reject("Hub connection is closed"); return }
        guard message.utf8.count <= 2 * 1024 * 1024 else { call.reject("Request exceeds 2 MiB", "REQUEST_TOO_LARGE"); return }
        sendQueue.async {
            let finished = DispatchSemaphore(value: 0)
            let completion = SocketSendCompletion(call)
            let done: (Bool) -> Void = { sent in
                completion.complete(sent ? nil : "Hub connection is closed")
                finished.signal()
            }
            if let task = active as? URLSessionWebSocketTask { task.send(.string(message)) { error in done(error == nil) } }
            else if let pinned = active as? PinnedSocket { pinned.send(message, completion: done) }
            else { done(false) }
            if finished.wait(timeout: .now() + 30) == .timedOut {
                self.closeSocket(active, .goingAway)
                completion.complete("Hub socket send timed out")
            }
        }
    }
    @objc public func disconnect(_ call: CAPPluginCall) { _ = supersede(); call.resolve() }
    @objc public func removeHub(_ call: CAPPluginCall) {
        do {
            let id = call.getString("id") ?? ""
            hubUpdateLock.lock(); defer { hubUpdateLock.unlock() }
            var hubs = try vault(); hubs.removeValue(forKey: id); try save(hubs)
            if activeHub == id { _ = supersede() }
            call.resolve()
        } catch { call.reject(error.localizedDescription) }
    }
}


private enum InvitationScanFailure: Error { case cancelled, unavailable }
/** Capture starts after presentation so an unavailable camera can always be dismissed. */
private final class InvitationScanner: UIViewController, AVCaptureMetadataOutputObjectsDelegate, UIAdaptivePresentationControllerDelegate {
    private let session = AVCaptureSession()
    private let queue = DispatchQueue(label: "tools.twr.qr-camera")
    private var preview: AVCaptureVideoPreviewLayer?
    private var rotationCoordinator: AVCaptureDevice.RotationCoordinator?
    private var rotationObservation: NSKeyValueObservation?
    private var finished = false
    private var configured = false
    private let completion: (Result<String, InvitationScanFailure>) -> Void
    init(completion: @escaping (Result<String, InvitationScanFailure>) -> Void) { self.completion = completion; super.init(nibName: nil, bundle: nil) }
    required init?(coder: NSCoder) { fatalError("init(coder:) is unavailable") }
    override func viewDidLoad() {
        super.viewDidLoad(); view.backgroundColor = .black
        let cancel = UIButton(type: .system); cancel.setTitle("Cancel scan", for: .normal); cancel.backgroundColor = .systemBackground
        cancel.translatesAutoresizingMaskIntoConstraints = false; cancel.addTarget(self, action: #selector(cancelScan), for: .touchUpInside)
        let layer = AVCaptureVideoPreviewLayer(session: session); layer.videoGravity = .resizeAspectFill; view.layer.addSublayer(layer); preview = layer
        view.addSubview(cancel)
        NSLayoutConstraint.activate([cancel.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16), cancel.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 16), cancel.widthAnchor.constraint(equalToConstant: 140), cancel.heightAnchor.constraint(equalToConstant: 44)])
        presentationController?.delegate = self
    }
    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated); guard !configured else { return }; configured = true
        queue.async { [weak self] in
            guard let self else { return }
            guard let device = AVCaptureDevice.default(for: .video), let input = try? AVCaptureDeviceInput(device: device), session.canAddInput(input) else { DispatchQueue.main.async { self.finish(.failure(.unavailable)) }; return }
            session.addInput(input)
            DispatchQueue.main.async {
                guard let preview = self.preview, !self.finished else { return }
                let coordinator = AVCaptureDevice.RotationCoordinator(device: device, previewLayer: preview)
                self.rotationCoordinator = coordinator
                self.rotationObservation = coordinator.observe(\.videoRotationAngleForHorizonLevelPreview, options: [.initial, .new]) { [weak self] coordinator, _ in
                    DispatchQueue.main.async {
                        guard let connection = self?.preview?.connection else { return }
                        let angle = coordinator.videoRotationAngleForHorizonLevelPreview
                        if connection.isVideoRotationAngleSupported(angle) { connection.videoRotationAngle = angle }
                    }
                }
            }
            let output = AVCaptureMetadataOutput()
            guard session.canAddOutput(output) else { DispatchQueue.main.async { self.finish(.failure(.unavailable)) }; return }
            session.addOutput(output)
            guard output.availableMetadataObjectTypes.contains(.qr) else { DispatchQueue.main.async { self.finish(.failure(.unavailable)) }; return }
            output.setMetadataObjectsDelegate(self, queue: .main); output.metadataObjectTypes = [.qr]; session.startRunning()
        }
    }
    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews(); preview?.frame = view.bounds
        if let angle = rotationCoordinator?.videoRotationAngleForHorizonLevelPreview, let connection = preview?.connection, connection.isVideoRotationAngleSupported(angle) { connection.videoRotationAngle = angle }
    }
    @objc private func cancelScan() { finish(.failure(.cancelled)) }
    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) { finish(.failure(.cancelled)) }
    func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject], from connection: AVCaptureConnection) {
        guard let value = metadataObjects.compactMap({ ($0 as? AVMetadataMachineReadableCodeObject)?.stringValue }).first else { return }; finish(.success(value))
    }
    private func finish(_ result: Result<String, InvitationScanFailure>) {
        guard !finished else { return }; finished = true
        rotationObservation = nil; rotationCoordinator = nil
        queue.async { [session] in if session.isRunning { session.stopRunning() } }
        if presentingViewController != nil { dismiss(animated: true) { [completion] in completion(result) } }
        else { completion(result) }
    }
}
