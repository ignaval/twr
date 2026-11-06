import Foundation
import Security
import Capacitor

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

@objc(TwrTransportPlugin)
public final class TwrTransportPlugin: CAPPlugin, CAPBridgedPlugin, URLSessionTaskDelegate {
    public let identifier = "TwrTransportPlugin"
    public let jsName = "TwrTransport"
    public let pluginMethods: [CAPPluginMethod] = ["listHubs", "pair", "connect", "send", "disconnect", "removeHub"].map { CAPPluginMethod(name: $0, returnType: CAPPluginReturnPromise) }
    private let connectionLock=NSLock()
    private var currentSocket: URLSessionWebSocketTask?
    private var socket: URLSessionWebSocketTask? {
        get {connectionLock.lock();defer{connectionLock.unlock()};return currentSocket}
        set {connectionLock.lock();currentSocket=newValue;connectionLock.unlock()}
    }
    private var activeHub: String?
    private let service = "tools.twr.mobile.hubs"
    private let storageLock = NSLock()
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
    private func endpoint(_ value: String) throws -> URL {
        guard let url = URL(string: value), let scheme = url.scheme, let host = url.host, url.user == nil, url.password == nil, url.query == nil, url.fragment == nil, url.path.isEmpty || url.path == "/" else { throw Failure("Enter a valid hub URL") }
        var local = false
        #if DEBUG
        local = ["localhost", "127.0.0.1", "::1"].contains(host)
        #endif
        guard scheme == "https" || (scheme == "http" && local) else { throw Failure("Use an HTTPS hub URL") }
        return url
    }
    private func metadata(_ id: String, _ hub: [String: Any]) -> [String: Any] { ["id": id, "name": hub["name"] ?? "My hub", "url": hub["url"] ?? "", "scopes": hub["scopes"] ?? [], "gateway_id": hub["gateway_id"] ?? ""] }
    private struct Failure: LocalizedError { let message: String; init(_ message: String) { self.message = message }; var errorDescription: String? { message } }
    @objc public func listHubs(_ call: CAPPluginCall) { do { call.resolve(["hubs": try vault().map { metadata($0.key, $0.value) }]) } catch { call.reject(error.localizedDescription) } }
    @objc public func pair(_ call: CAPPluginCall) {
        Task {
            do {
                let url = try endpoint(call.getString("url") ?? "")
                var request = URLRequest(url: url.appendingPathComponent("v1/pair")); request.httpMethod = "POST"; request.setValue("application/json", forHTTPHeaderField: "Content-Type")
                request.httpBody = try JSONSerialization.data(withJSONObject: ["code": call.getString("code") ?? "", "name": call.getString("name") ?? ""])
                let (data, response) = try await network.data(for: request)
                guard (response as? HTTPURLResponse)?.statusCode == 200, var record = try JSONSerialization.jsonObject(with: data) as? [String: Any], let token = record["token"] as? String, token.count == 64 else { throw Failure("Pairing failed. Check the code and hub URL.") }
                let id = UUID().uuidString; record["url"] = url.absoluteString; record["name"] = call.getString("hubName") ?? "My hub"
                var hubs = try vault(); hubs[id] = record; try save(hubs)
                call.resolve(metadata(id, record))
            } catch { call.reject(error.localizedDescription) }
        }
    }
    @objc public func connect(_ call: CAPPluginCall) {
        do {
            let id = call.getString("id") ?? ""
            guard let hub = try vault()[id], let rawURL = hub["url"] as? String, let token = hub["token"] as? String else { throw Failure("Paired hub not found") }
            let base = try endpoint(rawURL)
            var components = URLComponents(url: base.appendingPathComponent("v1/socket"), resolvingAgainstBaseURL: false)!
            components.scheme = base.scheme == "https" ? "wss" : "ws"
            socket?.cancel(with: .normalClosure, reason: nil)
            var request=URLRequest(url:components.url!);request.setValue("Bearer \(token)",forHTTPHeaderField:"Authorization")
            let task = socketNetwork.webSocketTask(with: request); task.maximumMessageSize=16*1024*1024; socket = task; activeHub = id; task.resume()
            Task {
                var authenticated = false
                do {
                    while !Task.isCancelled {
                        let frame = try await task.receive()
                        guard case .string(let text) = frame, text.utf8.count <= 16 * 1024 * 1024, let data = text.data(using: .utf8), let message = try JSONSerialization.jsonObject(with: data) as? [String: Any] else { throw Failure("Invalid hub response") }
                        guard self.socket === task else { if !authenticated { call.reject("Connection was superseded") }; return }
                        if message["type"] as? String == "authenticated" { authenticated = true; call.resolve() }
                        self.notifyListeners("message", data: ["message": message])
                    }
                } catch {
                    if !authenticated { call.reject("Could not authenticate with the hub") }
                    if self.socket === task { self.notifyListeners("message", data: ["message": ["type": "transport-closed", "connected": false, "error": "Remote hub connection closed"]]) }
                }
            }
        } catch { call.reject(error.localizedDescription) }
    }
    @objc public func send(_ call: CAPPluginCall) {
        guard let task = socket, let message = call.getString("message"), message.utf8.count <= 2 * 1024 * 1024 else { call.reject("Hub connection is closed"); return }
        sendQueue.async {
            let finished = DispatchSemaphore(value: 0)
            let completion = SocketSendCompletion(call)
            task.send(.string(message)) { error in
                completion.complete(error == nil ? nil : "Hub connection is closed")
                finished.signal()
            }
            if finished.wait(timeout: .now() + 30) == .timedOut {
                task.cancel(with: .goingAway, reason: nil)
                completion.complete("Hub socket send timed out")
            }
        }
    }
    @objc public func disconnect(_ call: CAPPluginCall) { socket?.cancel(with: .normalClosure, reason: nil); socket = nil; call.resolve() }
    @objc public func removeHub(_ call: CAPPluginCall) { do { let id = call.getString("id") ?? ""; var hubs = try vault(); hubs.removeValue(forKey: id); try save(hubs); if activeHub == id { socket?.cancel(with: .normalClosure, reason: nil); socket = nil }; call.resolve() } catch { call.reject(error.localizedDescription) } }
}
