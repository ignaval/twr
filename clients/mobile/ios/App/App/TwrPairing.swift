import Foundation
import CryptoKit
import TwrArgon2

// The pure parts of certificate-free phone pairing (docs/design/phone-pairing.md): codes,
// pins, the typed-code pin proof, routes and their bookkeeping, and the HTTP/1.1 and
// WebSocket wire formats the pinned transport speaks. Nothing here does I/O; AppTests'
// PairingLogicTests checks it against clients/shared/fixtures/phone-pairing-v2.json.

enum Pairing {
    static let salt = Data("twr-mobile-pair-v2".utf8)
    static let proofInfo = Data("pin-proof".utf8)
    /// "twr-mobile-pin-v2" || 0x00
    static let proofLabel = Data("twr-mobile-pin-v2".utf8) + Data([0])
    static let codeAlphabet = Array("ABCDEFGHJKLMNPQRSTUVWXYZ23456789".utf8)
    static let codeLength = 12
    static let nonceLength = 32
    static let defaultPort: UInt16 = 47321

    /// ASCII spaces and `-` removed and ASCII letters uppercased, if what is left is exactly a
    /// 12-symbol code; nil otherwise.
    static func normalizeCode(_ input: String) -> String? {
        var bytes: [UInt8] = []
        for byte in input.utf8 where byte != 0x20 && byte != 0x2d {
            bytes.append(byte >= 0x61 && byte <= 0x7a ? byte - 0x20 : byte)
        }
        guard bytes.count == codeLength, bytes.allSatisfy({ codeAlphabet.contains($0) }) else { return nil }
        return String(decoding: bytes, as: UTF8.self)
    }

    // MARK: base64url without padding

    private static let base64URLAlphabet = Array("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_".utf8)

    static func base64URL(_ data: Data) -> String {
        data.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }

    /// Strict, as the hub decodes: no padding, no whitespace, no other alphabet, zero
    /// leftover bits.
    static func base64URLDecode(_ text: String) -> Data? {
        let characters = Array(text.utf8)
        guard characters.count % 4 != 1 else { return nil }
        var out = Data(capacity: characters.count * 3 / 4)
        var accumulator: UInt32 = 0
        var bits: UInt32 = 0
        for character in characters {
            guard let value = base64URLAlphabet.firstIndex(of: character) else { return nil }
            accumulator = (accumulator << 6) | UInt32(value)
            bits += 6
            if bits >= 8 {
                bits -= 8
                out.append(UInt8(truncatingIfNeeded: accumulator >> bits))
                accumulator &= (1 << bits) - 1
            }
        }
        return accumulator == 0 ? out : nil
    }

    // MARK: pins

    /// The DER SubjectPublicKeyInfo header of an uncompressed P-256 key; the 65-byte point
    /// (0x04 || X || Y) follows it.
    static let p256SPKIPrefix = Data([
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01,
        0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ])

    /// The SPKI DER for an uncompressed P-256 point, as `SecKeyCopyExternalRepresentation`
    /// gives it; nil for anything else.
    static func p256SPKI(point: Data) -> Data? {
        guard point.count == 65, point.first == 0x04 else { return nil }
        return p256SPKIPrefix + point
    }

    /// The raw pin: SHA-256 of the SPKI DER.
    static func pin(spki: Data) -> Data { Data(SHA256.hash(data: spki)) }

    /// A pin as invitations, `/health` and the saved hub carry it (43 characters) to its 32 bytes.
    static func decodePin(_ text: String) -> Data? {
        guard let bytes = base64URLDecode(text), bytes.count == 32 else { return nil }
        return bytes
    }

    // MARK: the typed-code pin proof

    /// Argon2id(code, "twr-mobile-pair-v2", 64 MiB, 3 passes, 1 lane, v0x13, 32 bytes) of the
    /// normalized code. Takes a fraction of a second and 64 MiB: never call it on the main thread.
    static func stretch(code: String) -> Data? {
        guard let code = normalizeCode(code) else { return nil }
        let password = Array(code.utf8)
        let saltBytes = Array(salt)
        var out = [UInt8](repeating: 0, count: 32)
        let status = twr_argon2id(password, password.count, saltBytes, saltBytes.count, 3, 65536, 1, &out, out.count)
        return status == 0 ? Data(out) : nil
    }

    /// K_proof = HKDF-SHA256(salt "twr-mobile-pair-v2", ikm stretched, info "pin-proof", 32 bytes).
    static func proofKey(stretched: Data) -> SymmetricKey {
        HKDF<SHA256>.deriveKey(inputKeyMaterial: SymmetricKey(data: stretched), salt: salt, info: proofInfo, outputByteCount: 32)
    }

    /// HMAC-SHA256(K_proof, label || 0x00 || pin || nonce || gateway_id); nil for a pin or a
    /// nonce of the wrong length.
    static func proof(key: SymmetricKey, pin: Data, nonce: Data, gatewayID: String) -> Data? {
        guard pin.count == 32, nonce.count == nonceLength else { return nil }
        var message = proofLabel
        message.append(pin)
        message.append(nonce)
        message.append(Data(gatewayID.utf8))
        return Data(HMAC<SHA256>.authenticationCode(for: message, using: key))
    }

    /// Checks a proof in constant time.
    static func verifyProof(_ proof: Data, key: SymmetricKey, pin: Data, nonce: Data, gatewayID: String) -> Bool {
        guard let expected = self.proof(key: key, pin: pin, nonce: nonce, gatewayID: gatewayID) else { return false }
        return constantTimeEqual(expected, proof)
    }

    /// Equal lengths and contents, in time that depends only on the length.
    static func constantTimeEqual(_ a: Data, _ b: Data) -> Bool {
        guard a.count == b.count else { return false }
        var difference: UInt8 = 0
        for (x, y) in zip(a, b) { difference |= x ^ y }
        return difference == 0
    }

    // MARK: addresses

    /// Four decimal octets, no leading zeros.
    static func ipv4(_ text: String) -> [UInt8]? {
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return nil }
        var bytes: [UInt8] = []
        for part in parts {
            guard (1...3).contains(part.count), part.utf8.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }),
                  part.count == 1 || part.first != "0", let value = UInt8(part) else { return nil }
            bytes.append(value)
        }
        return bytes
    }

    /// An IPv6 literal without brackets or zone.
    static func ipv6(_ text: String) -> [UInt8]? {
        guard text.contains(":"), !text.contains("%") else { return nil }
        var address = in6_addr()
        guard text.withCString({ inet_pton(AF_INET6, $0, &address) }) == 1 else { return nil }
        return withUnsafeBytes(of: &address) { Array($0) }
    }

    /// A DNS name as the hub accepts one: labels of ASCII letters, digits and inner hyphens,
    /// with at least one letter somewhere (so `12345` is not a name).
    static func validDNSName(_ name: String) -> Bool {
        guard !name.isEmpty, name.utf8.count <= 253, name.utf8.contains(where: isASCIILetter) else { return false }
        return name.split(separator: ".", omittingEmptySubsequences: false).allSatisfy { label in
            !label.isEmpty && label.utf8.count <= 63 && label.first != "-" && label.last != "-"
                && label.utf8.allSatisfy { isASCIILetter($0) || ($0 >= 0x30 && $0 <= 0x39) || $0 == 0x2d }
        }
    }

    private static func isASCIILetter(_ byte: UInt8) -> Bool { (byte >= 0x41 && byte <= 0x5a) || (byte >= 0x61 && byte <= 0x7a) }

    /// A host as routes carry it: an IPv4 literal, an IPv6 literal (unbracketed), or a DNS
    /// name, lowercased; nil for anything else.
    static func canonicalHost(_ host: String) -> String? {
        if ipv4(host) != nil { return host }
        if ipv6(host) != nil { return host.lowercased() }
        return validDNSName(host) ? host.lowercased() : nil
    }

    /// What racing skips on cellular: RFC 1918, fc00::/7 outside the tailnet's
    /// fd7a:115c:a1e0::/48, and `.local` names.
    static func isPrivateLAN(host: String) -> Bool {
        if let v4 = ipv4(host) {
            return v4[0] == 10 || (v4[0] == 172 && (v4[1] & 0xf0) == 16) || (v4[0] == 192 && v4[1] == 168)
        }
        if let v6 = ipv6(host) {
            guard (v6[0] & 0xfe) == 0xfc else { return false }
            return Array(v6[0..<6]) != [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0]
        }
        var name = host.lowercased()
        if name.hasSuffix(".") { name.removeLast() }
        return name == "local" || name.hasSuffix(".local")
    }

    /// A typed hub address, `host` or `host:port` (`[IPv6]` or `[IPv6]:port`; a bare IPv6
    /// literal takes the default port), as a `tls` route.
    static func typedAddress(_ input: String) -> PairingRoute? {
        let text = input.trimmingCharacters(in: .whitespaces)
        guard !text.isEmpty, text.utf8.count <= 255 else { return nil }
        if ipv6(text) != nil { return .tls(host: text.lowercased(), port: defaultPort) }
        if text.hasPrefix("["), let close = text.firstIndex(of: "]") {
            let after = text[text.index(after: close)...]
            if after.isEmpty { return PairingRoute.parseHostPort("\(text):\(defaultPort)") }
        } else if !text.contains(":") {
            return PairingRoute.parseHostPort("\(text):\(defaultPort)")
        }
        return PairingRoute.parseHostPort(text)
    }
}

/// One entry of a hub's route list, `<kind>:<rest>`.
enum PairingRoute: Equatable {
    /// Direct TLS, authenticated by the pin. `host` is an IPv4 literal, an unbracketed IPv6
    /// literal or a lowercased DNS name.
    case tls(host: String, port: UInt16)
    /// A kind this version doesn't know (`relay:` in phase 1): kept, never dialed.
    case other(String)

    init?(_ text: String) {
        guard !text.isEmpty, text.utf8.count <= 255, let colon = text.firstIndex(of: ":") else { return nil }
        let kind = text[..<colon]
        guard !kind.isEmpty, kind.utf8.allSatisfy({ $0 >= 0x61 && $0 <= 0x7a }) else { return nil }
        guard kind == "tls" else { self = .other(text); return }
        guard let route = PairingRoute.parseHostPort(String(text[text.index(after: colon)...])) else { return nil }
        self = route
    }

    /// `host:port` or `[IPv6]:port` as a `tls` route.
    static func parseHostPort(_ text: String) -> PairingRoute? {
        guard let colon = text.lastIndex(of: ":") else { return nil }
        let rawHost = String(text[..<colon])
        guard let port = UInt16(text[text.index(after: colon)...]), port != 0 else { return nil }
        if rawHost.hasPrefix("[") {
            guard rawHost.hasSuffix("]"), rawHost.count >= 2 else { return nil }
            let inner = String(rawHost.dropFirst().dropLast())
            guard Pairing.ipv6(inner) != nil else { return nil }
            return .tls(host: inner.lowercased(), port: port)
        }
        guard Pairing.ipv6(rawHost) == nil, let host = Pairing.canonicalHost(rawHost) else { return nil }
        return .tls(host: host, port: port)
    }

    /// The canonical text, as the hub prints it.
    var text: String {
        switch self {
        case .tls(let host, let port): return host.contains(":") ? "tls:[\(host)]:\(port)" : "tls:\(host):\(port)"
        case .other(let text): return text
        }
    }

    /// `host:port`, IPv6 bracketed: the Host header and the display URL's authority.
    var authority: String? {
        guard case .tls(let host, let port) = self else { return nil }
        return host.contains(":") ? "[\(host)]:\(port)" : "\(host):\(port)"
    }

    var isPrivateLAN: Bool {
        guard case .tls(let host, _) = self else { return false }
        return Pairing.isPrivateLAN(host: host)
    }
}

/// A saved hub's routes: at most 16, deduplicated, each dropped 30 days after it was last seen.
struct SavedRoute: Equatable {
    var route: String
    /// Milliseconds since the epoch.
    var seen: Double
}

enum RouteBook {
    static let limit = 16
    static let lifetime: Double = 30 * 24 * 60 * 60 * 1000
    static let raceDelay: TimeInterval = 0.25

    /// From the Keychain record (`[{"route": …, "seen": …}]`); unparseable entries are dropped.
    static func decode(_ value: Any?) -> [SavedRoute] {
        guard let entries = value as? [[String: Any]] else { return [] }
        return entries.compactMap { entry in
            guard let text = entry["route"] as? String, let route = PairingRoute(text) else { return nil }
            return SavedRoute(route: route.text, seen: (entry["seen"] as? NSNumber)?.doubleValue ?? 0)
        }
    }

    static func encode(_ routes: [SavedRoute]) -> [[String: Any]] {
        routes.map { ["route": $0.route, "seen": $0.seen] }
    }

    /// `incoming` (an invitation's routes, a winning route, or what the gateway's hello
    /// reports) marked seen `now` and merged into `saved`, keeping the saved order and adding
    /// new routes at the end; then the expired dropped, and past the limit the least recently
    /// seen.
    static func merge(_ saved: [SavedRoute], _ incoming: [String], now: Double) -> [SavedRoute] {
        var routes: [SavedRoute] = []
        var known = Set<String>()
        for entry in saved where known.insert(entry.route).inserted { routes.append(entry) }
        for text in incoming {
            guard let route = PairingRoute(text)?.text else { continue }
            if let index = routes.firstIndex(where: { $0.route == route }) {
                routes[index].seen = max(routes[index].seen, now)
            } else {
                routes.append(SavedRoute(route: route, seen: now))
            }
        }
        routes.removeAll { now - $0.seen > lifetime }
        if routes.count > limit {
            let keep = Set(routes.indices.sorted { a, b in
                routes[a].seen != routes[b].seen ? routes[a].seen > routes[b].seen : a < b
            }.prefix(limit))
            routes = routes.indices.filter { keep.contains($0) }.map { routes[$0] }
        }
        return routes
    }

    struct RaceEntry: Equatable {
        let route: PairingRoute
        /// Starts `raceDelay` after the others.
        let delayed: Bool
    }

    /// The `tls` routes to race, in order: the remembered route for this network type at
    /// once, the rest after `raceDelay` (all at once when none is remembered). On cellular,
    /// private LAN routes are skipped, unless they are all there is (a VPN may reach them).
    static func racePlan(_ routes: [String], remembered: String?, cellular: Bool) -> [RaceEntry] {
        var candidates: [PairingRoute] = []
        for text in routes {
            guard let route = PairingRoute(text), case .tls = route, !candidates.contains(route) else { continue }
            candidates.append(route)
        }
        if cellular {
            let reachable = candidates.filter { !$0.isPrivateLAN }
            if !reachable.isEmpty { candidates = reachable }
        }
        guard let remembered = remembered.flatMap(PairingRoute.init), let first = candidates.firstIndex(of: remembered) else {
            return candidates.map { RaceEntry(route: $0, delayed: false) }
        }
        let lead = candidates.remove(at: first)
        return [RaceEntry(route: lead, delayed: false)] + candidates.map { RaceEntry(route: $0, delayed: true) }
    }
}

// MARK: - HTTP/1.1

struct HTTPResponse {
    let status: Int
    /// Lowercased names; repeated headers joined with ", ".
    let headers: [String: String]
    let body: Data
}

enum HTTPWire {
    static let maxHead = 16 * 1024

    /// A request that closes the connection after the response.
    static func request(method: String, path: String, authority: String, headers: [(String, String)] = [], body: Data? = nil) -> Data {
        var head = "\(method) \(path) HTTP/1.1\r\nHost: \(authority)\r\n"
        for (name, value) in headers { head += "\(name): \(value)\r\n" }
        if let body { head += "Content-Length: \(body.count)\r\n" }
        head += "Connection: close\r\n\r\n"
        var data = Data(head.utf8)
        if let body { data.append(body) }
        return data
    }

    struct Head {
        let status: Int
        let headers: [String: String]
        /// Bytes of the status line and headers, the blank line included.
        let length: Int
    }

    enum Failure: Error { case malformed, tooLarge }

    /// The response head once `buffer` holds all of it; nil while more is needed.
    static func parseHead(_ buffer: [UInt8]) throws -> Head? {
        guard let end = find([13, 10, 13, 10], in: buffer, from: 0) else {
            if buffer.count > maxHead { throw Failure.tooLarge }
            return nil
        }
        guard end <= maxHead else { throw Failure.tooLarge }
        let lines = String(decoding: buffer[..<end], as: UTF8.self).components(separatedBy: "\r\n")
        let status = lines[0].split(separator: " ", maxSplits: 2, omittingEmptySubsequences: true)
        guard status.count >= 2, status[0].hasPrefix("HTTP/1."), status[1].count == 3, let code = Int(status[1]) else { throw Failure.malformed }
        var headers: [String: String] = [:]
        for line in lines.dropFirst() {
            guard let colon = line.firstIndex(of: ":") else { continue }
            let name = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
            let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            headers[name] = headers[name].map { "\($0), \(value)" } ?? value
        }
        return Head(status: code, headers: headers, length: end + 4)
    }

    /// The whole response once `buffer` holds it (a Content-Length body, a chunked body, or
    /// everything up to end of stream when `eof`); nil while more is needed.
    static func parseResponse(_ buffer: [UInt8], eof: Bool) throws -> HTTPResponse? {
        guard let head = try parseHead(buffer) else {
            if eof { throw Failure.malformed }
            return nil
        }
        let rest = Array(buffer[head.length...])
        if head.status == 204 || head.status == 304 || (100..<200).contains(head.status) {
            return HTTPResponse(status: head.status, headers: head.headers, body: Data())
        }
        if head.headers["transfer-encoding"]?.lowercased().contains("chunked") == true {
            guard let body = try dechunk(rest) else {
                if eof { throw Failure.malformed }
                return nil
            }
            return HTTPResponse(status: head.status, headers: head.headers, body: body)
        }
        if let lengthText = head.headers["content-length"] {
            guard let length = Int(lengthText), length >= 0 else { throw Failure.malformed }
            guard rest.count >= length else {
                if eof { throw Failure.malformed }
                return nil
            }
            return HTTPResponse(status: head.status, headers: head.headers, body: Data(rest[..<length]))
        }
        return eof ? HTTPResponse(status: head.status, headers: head.headers, body: Data(rest)) : nil
    }

    /// A chunked body up to its last chunk; nil while more is needed. Trailers are ignored.
    static func dechunk(_ bytes: [UInt8]) throws -> Data? {
        var body = Data()
        var offset = 0
        while true {
            guard let lineEnd = find([13, 10], in: bytes, from: offset) else { return nil }
            let line = String(decoding: bytes[offset..<lineEnd], as: UTF8.self)
            let sizeText = line.split(separator: ";", maxSplits: 1, omittingEmptySubsequences: false)[0].trimmingCharacters(in: .whitespaces)
            guard !sizeText.isEmpty, sizeText.count <= 8, let size = Int(sizeText, radix: 16) else { throw Failure.malformed }
            offset = lineEnd + 2
            if size == 0 { return body }
            guard bytes.count >= offset + size + 2 else { return nil }
            guard bytes[offset + size] == 13, bytes[offset + size + 1] == 10 else { throw Failure.malformed }
            body.append(contentsOf: bytes[offset..<(offset + size)])
            offset += size + 2
        }
    }

    static func find(_ needle: [UInt8], in haystack: [UInt8], from start: Int) -> Int? {
        guard haystack.count >= needle.count, start <= haystack.count - needle.count else { return nil }
        var index = start
        while index <= haystack.count - needle.count {
            if haystack[index] == needle[0] && Array(haystack[index..<(index + needle.count)]) == needle { return index }
            index += 1
        }
        return nil
    }
}

// MARK: - WebSocket (RFC 6455), client side, no extensions

enum WebSocketWire {
    static let guid = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

    static func newKey() -> String {
        Data((0..<16).map { _ in UInt8.random(in: 0...255) }).base64EncodedString()
    }

    /// The Sec-WebSocket-Accept a server must answer `key` with.
    static func accept(for key: String) -> String {
        Data(Insecure.SHA1.hash(data: Data((key + guid).utf8))).base64EncodedString()
    }

    /// One whole frame (FIN set), masked as a client must.
    static func frame(opcode: UInt8, payload: [UInt8], mask: [UInt8]) -> [UInt8] {
        precondition(mask.count == 4)
        var frame: [UInt8] = [0x80 | opcode]
        let count = payload.count
        if count < 126 {
            frame.append(0x80 | UInt8(count))
        } else if count <= 0xffff {
            frame.append(0x80 | 126)
            frame.append(UInt8(count >> 8)); frame.append(UInt8(count & 0xff))
        } else {
            frame.append(0x80 | 127)
            for shift in stride(from: 56, through: 0, by: -8) { frame.append(UInt8((UInt64(count) >> UInt64(shift)) & 0xff)) }
        }
        frame.append(contentsOf: mask)
        frame.reserveCapacity(frame.count + count)
        for (index, byte) in payload.enumerated() { frame.append(byte ^ mask[index & 3]) }
        return frame
    }

    /// A close frame's payload: the code, big-endian.
    static func closePayload(_ code: UInt16) -> [UInt8] { [UInt8(code >> 8), UInt8(code & 0xff)] }

    /// The opening handshake, offering no subprotocol and no extension.
    static func upgradeRequest(path: String, authority: String, key: String, headers: [(String, String)]) -> Data {
        var head = "GET \(path) HTTP/1.1\r\nHost: \(authority)\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
        head += "Sec-WebSocket-Key: \(key)\r\nSec-WebSocket-Version: 13\r\n"
        for (name, value) in headers { head += "\(name): \(value)\r\n" }
        head += "\r\n"
        return Data(head.utf8)
    }

    /// Whether `head` completes the handshake opened with `key`: 101, the upgrade headers,
    /// the right accept value, and no extension (none was offered).
    static func accepts(_ head: HTTPWire.Head, key: String) -> Bool {
        let connection = (head.headers["connection"] ?? "").lowercased()
            .split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
        return head.status == 101
            && head.headers["upgrade"]?.lowercased() == "websocket"
            && connection.contains("upgrade")
            && head.headers["sec-websocket-accept"] == accept(for: key)
            && head.headers["sec-websocket-extensions"] == nil
    }
}

/// Turns the bytes a server sends into messages, enforcing what a client must: unmasked
/// frames, no reserved bits, whole control frames of at most 125 bytes, continuations only
/// inside a fragmented message, UTF-8 text, and the message size limit.
struct WebSocketReader {
    enum Event: Equatable {
        case text(String)
        case binary
        case ping([UInt8])
        case pong
        case close(code: UInt16?)
    }
    enum Failure: Error, Equatable { case protocolError, tooLarge }

    let maxMessage: Int
    private var buffer: [UInt8] = []
    private var offset = 0
    private var message: [UInt8] = []
    private var messageOpcode: UInt8?

    init(maxMessage: Int) { self.maxMessage = maxMessage }

    mutating func feed(_ bytes: [UInt8]) throws -> [Event] {
        buffer.append(contentsOf: bytes)
        var events: [Event] = []
        while let event = try next() {
            if let event { events.append(event) }
        }
        if offset > 0 && (offset >= 64 * 1024 || offset == buffer.count) {
            buffer.removeFirst(offset)
            offset = 0
        }
        return events
    }

    /// nil when more bytes are needed; .some(nil) for a frame that completed no event.
    private mutating func next() throws -> Event?? {
        let available = buffer.count - offset
        guard available >= 2 else { return nil }
        let first = buffer[offset], second = buffer[offset + 1]
        let fin = first & 0x80 != 0, opcode = first & 0x0f
        guard first & 0x70 == 0, second & 0x80 == 0 else { throw Failure.protocolError }
        var length = UInt64(second & 0x7f)
        var headerLength = 2
        if length == 126 {
            guard available >= 4 else { return nil }
            length = UInt64(buffer[offset + 2]) << 8 | UInt64(buffer[offset + 3])
            headerLength = 4
        } else if length == 127 {
            guard available >= 10 else { return nil }
            length = 0
            for index in 0..<8 { length = length << 8 | UInt64(buffer[offset + 2 + index]) }
            guard length >> 63 == 0 else { throw Failure.protocolError }
            headerLength = 10
        }
        let control = opcode & 0x08 != 0
        if control {
            guard fin, length <= 125 else { throw Failure.protocolError }
        } else {
            switch opcode {
            case 0: guard messageOpcode != nil else { throw Failure.protocolError }
            case 1, 2: guard messageOpcode == nil else { throw Failure.protocolError }
            default: throw Failure.protocolError
            }
            // Refuse an oversized message before buffering it.
            guard length <= UInt64(maxMessage - message.count) else { throw Failure.tooLarge }
        }
        guard UInt64(available - headerLength) >= length else { return nil }
        let start = offset + headerLength, end = start + Int(length)
        let payload = Array(buffer[start..<end])
        offset = end
        if control {
            switch opcode {
            case 0x8:
                guard payload.count != 1 else { throw Failure.protocolError }
                let code: UInt16? = payload.count >= 2 ? UInt16(payload[0]) << 8 | UInt16(payload[1]) : nil
                return Event.close(code: code)
            case 0x9: return Event.ping(payload)
            case 0xA: return Event.pong
            default: throw Failure.protocolError
            }
        }
        if opcode != 0 { messageOpcode = opcode }
        message.append(contentsOf: payload)
        guard fin else { return .some(nil) }
        defer { message = []; messageOpcode = nil }
        if messageOpcode == 2 { return Event.binary }
        guard let text = String(bytes: message, encoding: .utf8) else { throw Failure.protocolError }
        return Event.text(text)
    }
}
