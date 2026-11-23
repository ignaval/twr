import Foundation
import Network
import Security

// Networking for hubs paired by pin (docs/design/phone-pairing.md, "Client trust rule"). It
// uses Network.framework, not URLSession: under App Transport Security a URLSession trust
// delegate can tighten trust but not loosen it, so a self-signed hub key can't be accepted
// there, and ATS doesn't apply to Network.framework. Every connection checks the leaf key's
// SPKI SHA-256 against the pin in the TLS verify block, and Network.framework reports
// `.ready` only after that check passed: nothing is sent before it. Certificate-mode hubs
// stay on URLSession with system trust (TwrTransportPlugin).

/// Why one route failed.
enum RouteError: Error, Equatable {
    case unreachable(String)
    case timedOut
    case pinMismatch
    case unsupportedKey
    case closed
    case tooLarge
    case badResponse(String)
    case otherHub
    case cancelled

    func reason(hubName: String) -> String {
        switch self {
        case .unreachable(let detail): return "unreachable (\(detail))"
        case .timedOut: return "timed out"
        case .pinMismatch: return "this is another device, not \(hubName) (PIN_MISMATCH)"
        case .unsupportedKey: return "the device there has no P-256 key, so it is not a twr hub"
        case .closed: return "the connection closed before an answer"
        case .tooLarge: return "the answer was too large"
        case .badResponse(let detail): return detail
        case .otherHub: return "another twr hub answered, not \(hubName)"
        case .cancelled: return "cancelled"
        }
    }
}

/// Every route that failed, in the order they were tried; the message names each and why.
struct RaceFailure: LocalizedError {
    let failures: [(route: String, error: RouteError)]
    let hubName: String

    var pinMismatch: Bool { failures.contains { $0.error == .pinMismatch } }

    var errorDescription: String? {
        let text: String
        if failures.isEmpty {
            text = "\(hubName) has no route this phone can use. Pair it again."
        } else {
            let detail = failures.map { "\($0.route): \($0.error.reason(hubName: hubName))" }.joined(separator: "; ")
            text = "Could not connect to \(hubName). \(detail)"
        }
        return String(((pinMismatch ? "PIN_MISMATCH: " : "") + text).prefix(800))
    }
}

/// The verify block's decision for one connection, and the key it saw.
final class PinCheck {
    /// The pin to require; nil only for typed pairing's first connection, which captures
    /// the presented key without trusting it.
    let expected: Data?
    private let lock = NSLock()
    private var presentedPin: Data?
    private var refusal: RouteError?

    init(expected: Data?) { self.expected = expected }

    var presented: Data? { lock.lock(); defer { lock.unlock() }; return presentedPin }
    /// Set when the verify block refused the connection.
    var refused: RouteError? { lock.lock(); defer { lock.unlock() }; return refusal }

    func evaluate(_ trust: SecTrust) -> Bool {
        let pin = PinnedTLS.leafPin(trust)
        lock.lock(); defer { lock.unlock() }
        presentedPin = pin
        guard let pin else { refusal = expected == nil ? .unsupportedKey : .pinMismatch; return false }
        if let expected, !Pairing.constantTimeEqual(pin, expected) { refusal = .pinMismatch; return false }
        return true
    }
}

enum PinnedTLS {
    private static let verifyQueue = DispatchQueue(label: "tools.twr.pin-verify")

    /// SHA-256 of the leaf certificate's SPKI; nil unless the key is P-256. The chain, the
    /// name and the validity dates are ignored on purpose: the pin is the hub's identity.
    static func leafPin(_ trust: SecTrust) -> Data? {
        guard let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate], let leaf = chain.first,
              let key = SecCertificateCopyKey(leaf),
              let attributes = SecKeyCopyAttributes(key) as? [String: Any],
              (attributes[kSecAttrKeyType as String] as? String) == (kSecAttrKeyTypeECSECPrimeRandom as String),
              (attributes[kSecAttrKeySizeInBits as String] as? NSNumber)?.intValue == 256,
              let point = SecKeyCopyExternalRepresentation(key, nil) as Data?,
              let spki = Pairing.p256SPKI(point: point) else { return nil }
        return Pairing.pin(spki: spki)
    }

    /// TLS 1.2 at least (1.3 when the hub offers it, as it does), HTTP/1.1 by ALPN, and the
    /// pin check in place of system trust.
    static func parameters(_ check: PinCheck) -> NWParameters {
        let tls = NWProtocolTLS.Options()
        let options = tls.securityProtocolOptions
        sec_protocol_options_set_min_tls_protocol_version(options, .TLSv12)
        sec_protocol_options_set_max_tls_protocol_version(options, .TLSv13)
        sec_protocol_options_add_tls_application_protocol(options, "http/1.1")
        sec_protocol_options_set_verify_block(options, { _, trust, complete in
            complete(check.evaluate(sec_trust_copy_ref(trust).takeRetainedValue()))
        }, verifyQueue)
        let tcp = NWProtocolTCP.Options()
        tcp.noDelay = true
        return NWParameters(tls: tls, tcp: tcp)
    }

    static func connection(_ route: PairingRoute, _ check: PinCheck) -> NWConnection? {
        guard case .tls(let host, let port) = route, let endpointPort = NWEndpoint.Port(rawValue: port) else { return nil }
        return NWConnection(host: NWEndpoint.Host(host), port: endpointPort, using: parameters(check))
    }

    static func describe(_ error: NWError) -> String {
        switch error {
        case .posix(let code):
            return String(cString: strerror(code.rawValue))
        case .dns(let code):
            // kDNSServiceErr_NoSuchName, kDNSServiceErr_NoSuchRecord, kDNSServiceErr_PolicyDenied
            switch code {
            case -65538, -65554: return "name not found"
            case -65570: return "local network access is off for twr in Settings"
            default: return "DNS error \(code)"
            }
        case .tls(let status):
            return (SecCopyErrorMessageString(status, nil) as String?) ?? "TLS error \(status)"
        @unknown default:
            return "\(error)"
        }
    }

    /// A refusal that waiting won't fix. Other waiting reasons (no network yet, the local
    /// network permission prompt still open) may clear before the deadline.
    static func isFinal(_ error: NWError) -> Bool {
        if case .posix(.ECONNREFUSED) = error { return true }
        return false
    }
}

// MARK: - HTTP/1.1 over pinned TLS

enum PinnedHTTP {
    static let responseLimit = 64 * 1024

    static func get(_ route: PairingRoute, check: PinCheck, path: String, timeout: TimeInterval) async throws -> HTTPResponse {
        guard let authority = route.authority else { throw RouteError.badResponse("not a direct route") }
        let request = HTTPWire.request(method: "GET", path: path, authority: authority, headers: [("Accept", "application/json")])
        return try await exchange(route, check: check, request: request, timeout: timeout)
    }

    static func postJSON(_ route: PairingRoute, check: PinCheck, path: String, body: Data, timeout: TimeInterval) async throws -> HTTPResponse {
        guard let authority = route.authority else { throw RouteError.badResponse("not a direct route") }
        let request = HTTPWire.request(method: "POST", path: path, authority: authority,
                                       headers: [("Accept", "application/json"), ("Content-Type", "application/json")], body: body)
        return try await exchange(route, check: check, request: request, timeout: timeout)
    }

    static func json(_ response: HTTPResponse) -> [String: Any]? {
        (try? JSONSerialization.jsonObject(with: response.body)) as? [String: Any]
    }

    /// One request on its own connection (`Connection: close`). Throws only RouteError.
    static func exchange(_ route: PairingRoute, check: PinCheck, request: Data, timeout: TimeInterval) async throws -> HTTPResponse {
        guard let connection = PinnedTLS.connection(route, check) else { throw RouteError.badResponse("not a direct route") }
        return try await HTTPExchange(connection: connection, check: check, request: request).run(timeout: timeout)
    }
}

/// One request and its response. Everything after init runs on `queue`.
private final class HTTPExchange {
    private let connection: NWConnection
    private let check: PinCheck
    private let request: Data
    private let queue = DispatchQueue(label: "tools.twr.pinned-http")
    private var buffer: [UInt8] = []
    private var continuation: CheckedContinuation<HTTPResponse, Error>?
    private var finished = false
    private var waitingReason: String?

    init(connection: NWConnection, check: PinCheck, request: Data) {
        self.connection = connection
        self.check = check
        self.request = request
    }

    func run(timeout: TimeInterval) async throws -> HTTPResponse {
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<HTTPResponse, Error>) in
                self.queue.async { self.start(continuation, timeout: timeout) }
            }
        } onCancel: {
            self.queue.async { self.finish(.failure(RouteError.cancelled)) }
        }
    }

    private func start(_ continuation: CheckedContinuation<HTTPResponse, Error>, timeout: TimeInterval) {
        guard !finished else { continuation.resume(throwing: RouteError.cancelled); return }
        self.continuation = continuation
        connection.stateUpdateHandler = { state in self.changed(state) }
        connection.start(queue: queue)
        queue.asyncAfter(deadline: .now() + timeout) {
            self.finish(.failure(self.check.refused ?? self.waitingReason.map(RouteError.unreachable) ?? RouteError.timedOut))
        }
    }

    private func changed(_ state: NWConnection.State) {
        switch state {
        case .ready:
            connection.send(content: request, completion: .contentProcessed { error in
                if let error { self.finish(.failure(self.failure(error))) }
            })
            receive()
        case .waiting(let error):
            if let refusal = check.refused { finish(.failure(refusal)); return }
            if PinnedTLS.isFinal(error) { finish(.failure(RouteError.unreachable(PinnedTLS.describe(error)))); return }
            waitingReason = PinnedTLS.describe(error)
        case .failed(let error):
            finish(.failure(failure(error)))
        default:
            break
        }
    }

    private func failure(_ error: NWError) -> RouteError { check.refused ?? .unreachable(PinnedTLS.describe(error)) }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) { data, _, isComplete, error in
            guard !self.finished else { return }
            if let data { self.buffer.append(contentsOf: data) }
            guard self.buffer.count <= PinnedHTTP.responseLimit else { self.finish(.failure(RouteError.tooLarge)); return }
            if isComplete && self.buffer.isEmpty { self.finish(.failure(RouteError.closed)); return }
            do {
                if let response = try HTTPWire.parseResponse(self.buffer, eof: isComplete) {
                    self.finish(.success(response))
                    return
                }
            } catch {
                self.finish(.failure(RouteError.badResponse("the answer is not HTTP")))
                return
            }
            if let error { self.finish(.failure(self.failure(error))); return }
            if isComplete { self.finish(.failure(RouteError.closed)); return }
            self.receive()
        }
    }

    private func finish(_ result: Result<HTTPResponse, Error>) {
        guard !finished else { return }
        finished = true
        connection.stateUpdateHandler = nil
        connection.cancel()
        continuation?.resume(with: result)
        continuation = nil
    }
}

// MARK: - Routes

enum HubProbe {
    struct Found {
        let route: PairingRoute
        let gatewayID: String
    }

    /// `GET /health`: a twr gateway, and the expected one when `expectedGateway` is set.
    static func health(_ route: PairingRoute, check: PinCheck, expectedGateway: String?, timeout: TimeInterval) async throws -> Found {
        let response = try await PinnedHTTP.get(route, check: check, path: "/health", timeout: timeout)
        guard response.status == 200, let health = PinnedHTTP.json(response), health["service"] as? String == "twr-mobile",
              let gatewayID = health["gateway_id"] as? String, !gatewayID.isEmpty else {
            throw RouteError.badResponse("not a twr gateway (\(response.status))")
        }
        if let expectedGateway, !expectedGateway.isEmpty, gatewayID != expectedGateway { throw RouteError.otherHub }
        return Found(route: route, gatewayID: gatewayID)
    }
}

enum RouteRace {
    /// The first route of `plan` to complete TLS with the pin and answer `/health` as the
    /// expected gateway; the others are cancelled.
    static func run(_ plan: [RouteBook.RaceEntry], pin: Data, gatewayID: String?, hubName: String, timeout: TimeInterval) async throws -> HubProbe.Found {
        guard !plan.isEmpty else { throw RaceFailure(failures: [], hubName: hubName) }
        return try await withThrowingTaskGroup(of: (Int, Result<HubProbe.Found, RouteError>).self) { group in
            for (index, entry) in plan.enumerated() {
                group.addTask {
                    if entry.delayed { try? await Task.sleep(nanoseconds: UInt64(RouteBook.raceDelay * 1_000_000_000)) }
                    if Task.isCancelled { return (index, .failure(.cancelled)) }
                    do {
                        let found = try await HubProbe.health(entry.route, check: PinCheck(expected: pin), expectedGateway: gatewayID, timeout: timeout)
                        return (index, .success(found))
                    } catch let error as RouteError {
                        return (index, .failure(error))
                    } catch {
                        return (index, .failure(.unreachable(error.localizedDescription)))
                    }
                }
            }
            var failures: [(Int, RouteError)] = []
            for try await (index, result) in group {
                switch result {
                case .success(let found):
                    group.cancelAll()
                    return found
                case .failure(let error):
                    failures.append((index, error))
                }
            }
            let ordered = failures.sorted { $0.0 < $1.0 }.map { (route: plan[$0.0].route.text, error: $0.1) }
            throw RaceFailure(failures: ordered, hubName: hubName)
        }
    }
}

// MARK: - The hub socket

/// `/v1/socket` over pinned TLS: the HTTP/1.1 upgrade, then RFC 6455 frames. Written out here
/// rather than with NWProtocolWebSocket so that a refused upgrade's status is visible (only
/// 401 means the token is revoked). Callbacks run on the socket's serial queue, in order.
final class PinnedSocket {
    enum End: Equatable {
        /// The upgrade was answered with this status instead of 101.
        case refused(Int)
        case pinMismatch
        case timedOut
        case closed
    }

    static let maxMessage = 16 * 1024 * 1024
    static let handshakeTimeout: TimeInterval = 15
    /// The gateway sends a heartbeat every 15 seconds; this long without a byte is a dead link.
    static let idleLimit: UInt64 = 120 * 1_000_000_000

    let route: PairingRoute
    /// Return false to close the socket (an invalid or unwanted message).
    var onText: ((PinnedSocket, String) -> Bool)?
    var onEnd: ((PinnedSocket, End) -> Void)?

    private let connection: NWConnection
    private let check: PinCheck
    private let queue = DispatchQueue(label: "tools.twr.pinned-socket")
    private let key: String
    private let upgrade: Data
    private var head: [UInt8] = []
    private var reader = WebSocketReader(maxMessage: PinnedSocket.maxMessage)
    private var open = false
    private var ended = false
    private var lastReceive = DispatchTime.now()
    private var watchdog: DispatchSourceTimer?

    init?(route: PairingRoute, pin: Data, token: String) {
        let check = PinCheck(expected: pin)
        guard let authority = route.authority, !token.isEmpty, token.utf8.allSatisfy({ $0 > 0x20 && $0 < 0x7f }),
              let connection = PinnedTLS.connection(route, check) else { return nil }
        let key = WebSocketWire.newKey()
        self.route = route
        self.check = check
        self.connection = connection
        self.key = key
        upgrade = WebSocketWire.upgradeRequest(path: "/v1/socket", authority: authority, key: key,
                                               headers: [("Authorization", "Bearer \(token)"), ("X-Twr-Framing", "1")])
    }

    func start() {
        queue.async {
            self.connection.stateUpdateHandler = { state in self.changed(state) }
            self.connection.start(queue: self.queue)
            self.queue.asyncAfter(deadline: .now() + PinnedSocket.handshakeTimeout) {
                if !self.open { self.end(.timedOut) }
            }
        }
    }

    /// Sends one text message; `completion` gets whether the stack took it.
    func send(_ text: String, completion: @escaping (Bool) -> Void) {
        queue.async {
            guard self.open, !self.ended else { completion(false); return }
            self.write(opcode: 0x1, Array(text.utf8)) { error in completion(error == nil) }
        }
    }

    /// Closes normally. `onEnd` still runs, with `.closed`.
    func close() { queue.async { self.end(.closed, closing: 1000) } }

    private func changed(_ state: NWConnection.State) {
        switch state {
        case .ready:
            connection.send(content: upgrade, completion: .contentProcessed { error in
                if error != nil { self.end(.closed) }
            })
            receive()
        case .waiting, .failed:
            end(check.refused == .pinMismatch || check.refused == .unsupportedKey ? .pinMismatch : .closed)
        default:
            break
        }
    }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 512 * 1024) { data, _, isComplete, error in
            guard !self.ended else { return }
            if let data, !data.isEmpty {
                self.lastReceive = .now()
                self.consume([UInt8](data))
            }
            guard !self.ended else { return }
            if isComplete || error != nil { self.end(.closed); return }
            self.receive()
        }
    }

    private func consume(_ incoming: [UInt8]) {
        var bytes = incoming
        if !open {
            head.append(contentsOf: bytes)
            let parsed: HTTPWire.Head?
            do { parsed = try HTTPWire.parseHead(head) } catch { end(.closed); return }
            guard let parsed else { return }
            guard parsed.status == 101 else { end(.refused(parsed.status)); return }
            guard WebSocketWire.accepts(parsed, key: key) else { end(.closed); return }
            open = true
            bytes = Array(head[parsed.length...])
            head = []
            startWatchdog()
        }
        let events: [WebSocketReader.Event]
        do {
            events = try reader.feed(bytes)
        } catch {
            end(.closed, closing: (error as? WebSocketReader.Failure) == .tooLarge ? 1009 : 1002)
            return
        }
        for event in events {
            guard !ended else { return }
            switch event {
            case .text(let text):
                if onText?(self, text) == false { end(.closed, closing: 1003) }
            case .binary:
                end(.closed, closing: 1003)
            case .ping(let payload):
                write(opcode: 0xA, payload)
            case .pong:
                break
            case .close(let code):
                end(.closed, closing: code.map { $0 == 1005 ? 1000 : $0 } ?? 1000)
            }
        }
    }

    private func startWatchdog() {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + 30, repeating: 30)
        timer.setEventHandler {
            if DispatchTime.now().uptimeNanoseconds - self.lastReceive.uptimeNanoseconds > PinnedSocket.idleLimit {
                self.end(.timedOut, closing: 1001)
            }
        }
        timer.resume()
        watchdog = timer
    }

    private func write(opcode: UInt8, _ payload: [UInt8], completion: ((NWError?) -> Void)? = nil) {
        let mask = (0..<4).map { _ in UInt8.random(in: 0...255) }
        connection.send(content: Data(WebSocketWire.frame(opcode: opcode, payload: payload, mask: mask)),
                        completion: .contentProcessed { error in completion?(error) })
    }

    /// Ends once: sends a close frame first when the socket is open and `code` is set.
    private func end(_ how: End, closing code: UInt16? = nil) {
        guard !ended else { return }
        ended = true
        watchdog?.cancel()
        watchdog = nil
        connection.stateUpdateHandler = nil
        if open, let code {
            write(opcode: 0x8, WebSocketWire.closePayload(code)) { _ in self.connection.cancel() }
            queue.asyncAfter(deadline: .now() + 2) { self.connection.cancel() }
        } else {
            connection.cancel()
        }
        let callback = onEnd
        onText = nil
        onEnd = nil
        callback?(self, how)
    }
}

// MARK: - The default network

/// The device's default network type, for `last` routes and the `network` event.
final class NetworkWatch {
    /// Called on a private queue when the default network changes, with its type:
    /// "wifi", "cellular", "other" or "none".
    var onChange: ((String) -> Void)?
    private let monitor = NWPathMonitor()
    private let queue = DispatchQueue(label: "tools.twr.network")
    private let lock = NSLock()
    private var kind = "other"
    private var identity: String?

    func start() {
        monitor.pathUpdateHandler = { [weak self] path in self?.update(path) }
        monitor.start(queue: queue)
    }

    /// "wifi", "cellular", "other" or "none".
    var current: String { lock.lock(); defer { lock.unlock() }; return kind }

    /// The key a saved hub's `last` uses for the current network.
    var slot: String { current == "none" ? "other" : current }

    private func update(_ path: NWPath) {
        let kind = NetworkWatch.kind(of: path)
        // Another Wi-Fi network on the same interface changes the gateways.
        let identity = ([kind] + path.availableInterfaces.prefix(1).map(\.name) + path.gateways.map { "\($0)" }).joined(separator: "|")
        lock.lock()
        let previous = self.identity
        self.identity = identity
        self.kind = kind
        lock.unlock()
        if previous != nil && previous != identity { onChange?(kind) }
    }

    static func kind(of path: NWPath) -> String {
        guard path.status == .satisfied else { return "none" }
        if path.usesInterfaceType(.wifi) { return "wifi" }
        if path.usesInterfaceType(.cellular) { return "cellular" }
        return "other"
    }
}
