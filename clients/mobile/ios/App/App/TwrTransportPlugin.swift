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

@objc(TwrTransportPlugin)
public final class TwrTransportPlugin: CAPPlugin, CAPBridgedPlugin, URLSessionTaskDelegate {
    public let identifier = "TwrTransportPlugin"
    public let jsName = "TwrTransport"
    public let pluginMethods: [CAPPluginMethod] = ["listHubs", "pair", "connect", "send", "disconnect", "removeHub", "scanInvitation"].map { CAPPluginMethod(name: $0, returnType: CAPPluginReturnPromise) }
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
    @objc public func pair(_ call: CAPPluginCall) {
        Task {
            do {
                let url = try endpoint(call.getString("url") ?? "")
                let (healthData, healthResponse) = try await network.data(from:url.appendingPathComponent("health"))
                guard (healthResponse as? HTTPURLResponse)?.statusCode == 200,
                      let health=try JSONSerialization.jsonObject(with:healthData) as? [String:Any] else {throw Failure("Could not identify the hub")}
                if let gateway=health["gateway_id"] as? String, !gateway.isEmpty,
                   try vault().values.contains(where:{$0["gateway_id"] as? String == gateway}) {
                    throw Failure("This hub is already paired. Use Revoke this device in its settings, or Forget this hub under Device access if it cannot connect, then pair again.")
                }
                var request = URLRequest(url: url.appendingPathComponent("v1/pair")); request.httpMethod = "POST"; request.setValue("application/json", forHTTPHeaderField: "Content-Type")
                request.httpBody = try JSONSerialization.data(withJSONObject: ["code": call.getString("code") ?? "", "name": call.getString("name") ?? ""])
                let (data, response) = try await network.data(for: request)
                if (response as? HTTPURLResponse)?.statusCode != 200 {
                    let detail=(try? JSONSerialization.jsonObject(with:data) as? [String:Any])?["error"] as? String
                    throw Failure(String((detail ?? "Pairing failed. Check the code and hub URL.").prefix(240)))
                }
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
            var request=URLRequest(url:components.url!);request.setValue("Bearer \(token)",forHTTPHeaderField:"Authorization");request.setValue("1",forHTTPHeaderField:"X-Twr-Framing")
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
                    // Only 401 means the gateway no longer knows this token. A 403 and
                    // 5xx are refusals the app retries like any other lost connection.
                    let status = (task.response as? HTTPURLResponse)?.statusCode
                    let revoked = status == 401
                    let refused = !authenticated && status != nil && status != 101
                    let message = revoked ? "AUTH_REVOKED: This device no longer has access to the hub. Forget it and pair again." : refused ? "Gateway refused the connection (\(status!))" : "Remote hub connection closed"
                    if !authenticated { call.reject(message, revoked ? "AUTH_REVOKED" : nil) }
                    if self.socket === task { self.notifyListeners("message", data: ["message": ["type": "transport-closed", "connected": false, "error": message]]) }
                }
            }
        } catch { call.reject(error.localizedDescription) }
    }
    @objc public func send(_ call: CAPPluginCall) {
        guard let task = socket, let message = call.getString("message") else { call.reject("Hub connection is closed"); return }
        guard message.utf8.count <= 2 * 1024 * 1024 else { call.reject("Request exceeds 2 MiB", "REQUEST_TOO_LARGE"); return }
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
