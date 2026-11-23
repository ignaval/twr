import XCTest
import CryptoKit
@testable import App

/// The pure phone-pairing logic (TwrPairing.swift) against the shared test vectors in
/// clients/shared/fixtures/phone-pairing-v2.json. The values are copied here because the test
/// bundle can't read the repository; scripts/test-argon2.sh fails if they drift from the fixture.
final class PairingLogicTests: XCTestCase {
    private let code = "ABCDEFGHJK23"
    private let spkiHex = "3059301306072a8648ce3d020106082a8648ce3d030107034200046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"
    private let pin = "XNJS-wzokyQ2-vjM0QQJgbie5K1rn-niorfnGqyyfNM"
    private let stretchedHex = "c22980f65afd8c1e89a814ad3a193f536e583b845f1ec71201c9572f4e5fbeb8"
    private let proofKeyHex = "713dc829861f4ba90543b45934cfb330a679aef9e69b8dea05bf556b38cd9535"
    private let nonce = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8"
    private let gatewayID = "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90"
    private let proof = "Egyxaz2WLerRcNY5eLCAXL5lk6wBC3tbW9ZaBiZ5OFs"

    private func bytes(_ hex: String) -> Data {
        var data = Data()
        var index = hex.startIndex
        while index < hex.endIndex {
            let next = hex.index(index, offsetBy: 2)
            data.append(UInt8(hex[index..<next], radix: 16)!)
            index = next
        }
        return data
    }

    private func hex(_ data: Data) -> String { data.map { String(format: "%02x", $0) }.joined() }

    func testCodesNormalize() {
        XCTAssertEqual(Pairing.normalizeCode("abcd-efgh-jk23"), "ABCDEFGHJK23")
        XCTAssertEqual(Pairing.normalizeCode(" AB CD EF GH JK 23 "), "ABCDEFGHJK23")
        for invalid in ["ABCDEFGHJK2", "ABCDEFGHJK230", "ABCDEFGHIK23", "ABCD_EFGH_JK23", "", "ÄBCDEFGHJK23"] {
            XCTAssertNil(Pairing.normalizeCode(invalid), invalid)
        }
    }

    func testBase64URLIsStrict() {
        for length in 0..<40 {
            let data = Data((0..<length).map { UInt8(truncatingIfNeeded: $0 * 37 + 11) })
            XCTAssertEqual(Pairing.base64URLDecode(Pairing.base64URL(data)), data)
        }
        XCTAssertNil(Pairing.base64URLDecode("AA=="))
        XCTAssertNil(Pairing.base64URLDecode("A+"))
        XCTAssertNil(Pairing.base64URLDecode("AB"), "nonzero padding bits")
        XCTAssertNil(Pairing.base64URLDecode("A"))
        XCTAssertEqual(Pairing.base64URL(Data([0xfb, 0xff])), "-_8")
        XCTAssertEqual(Pairing.decodePin(pin)?.count, 32)
        XCTAssertNil(Pairing.decodePin("AAAA"))
    }

    func testPinFromAP256Point() {
        let spki = bytes(spkiHex)
        XCTAssertEqual(Pairing.base64URL(Pairing.pin(spki: spki)), pin)
        // SecKeyCopyExternalRepresentation gives the 65-byte point; the SPKI is the fixed header and it.
        let point = spki.suffix(65)
        XCTAssertEqual(Pairing.p256SPKI(point: Data(point)), spki)
        XCTAssertNil(Pairing.p256SPKI(point: Data(point.dropFirst())), "64 bytes")
        var compressed = Data(point)
        compressed[compressed.startIndex] = 0x02
        XCTAssertNil(Pairing.p256SPKI(point: compressed), "not uncompressed")
        // CryptoKit agrees on the DER of the same key.
        let key = try? P256.Signing.PublicKey(x963Representation: Data(point))
        XCTAssertEqual(key?.derRepresentation, spki)
    }

    func testProofDerivationMatchesTheFixture() throws {
        let stretched = try XCTUnwrap(Pairing.stretch(code: "abcd-efgh-jk23"), "Argon2id failed")
        XCTAssertEqual(hex(stretched), stretchedHex)
        XCTAssertNil(Pairing.stretch(code: "not a code"))
        let key = Pairing.proofKey(stretched: stretched)
        XCTAssertEqual(key.withUnsafeBytes { hex(Data($0)) }, proofKeyHex)
        let pinBytes = try XCTUnwrap(Pairing.decodePin(pin))
        let nonceBytes = try XCTUnwrap(Pairing.base64URLDecode(nonce))
        let expected = try XCTUnwrap(Pairing.base64URLDecode(proof))
        XCTAssertEqual(Pairing.proof(key: key, pin: pinBytes, nonce: nonceBytes, gatewayID: gatewayID), expected)
        XCTAssertTrue(Pairing.verifyProof(expected, key: key, pin: pinBytes, nonce: nonceBytes, gatewayID: gatewayID))

        var otherPin = pinBytes
        otherPin[otherPin.startIndex] ^= 1
        XCTAssertFalse(Pairing.verifyProof(expected, key: key, pin: otherPin, nonce: nonceBytes, gatewayID: gatewayID), "another key")
        XCTAssertFalse(Pairing.verifyProof(expected, key: key, pin: pinBytes, nonce: Data(repeating: 8, count: 32), gatewayID: gatewayID), "another nonce")
        XCTAssertFalse(Pairing.verifyProof(expected, key: key, pin: pinBytes, nonce: nonceBytes, gatewayID: gatewayID + "x"), "another gateway")
        XCTAssertFalse(Pairing.verifyProof(expected.prefix(31), key: key, pin: pinBytes, nonce: nonceBytes, gatewayID: gatewayID), "short proof")
        XCTAssertNil(Pairing.proof(key: key, pin: pinBytes, nonce: Data(count: 16), gatewayID: gatewayID))
        let otherKey = Pairing.proofKey(stretched: Data(repeating: 0, count: 32))
        XCTAssertFalse(Pairing.verifyProof(expected, key: otherKey, pin: pinBytes, nonce: nonceBytes, gatewayID: gatewayID), "another code")
    }

    func testRoutesParse() {
        XCTAssertEqual(PairingRoute("tls:192.168.1.20:47321"), .tls(host: "192.168.1.20", port: 47321))
        XCTAssertEqual(PairingRoute("tls:[2001:db8::5]:47321"), .tls(host: "2001:db8::5", port: 47321))
        XCTAssertEqual(PairingRoute("tls:Box.Tail1234.ts.net:443"), .tls(host: "box.tail1234.ts.net", port: 443))
        XCTAssertEqual(PairingRoute("relay:anything"), .other("relay:anything"))
        for invalid in ["tls:1.2.3.4", "tls:1.2.3.4:0", "tls:[::1:47321", "tls:-bad-.example:1", "tls:12345:1", ":x", "TLS:1.2.3.4:1",
                        "tls:2001:db8::5:47321", "tls:host:65536", "tls:[]:1", "tls:host.example:+80", ""] {
            XCTAssertNil(PairingRoute(invalid), invalid)
        }
        XCTAssertEqual(PairingRoute("tls:[2001:db8::5]:47321")?.text, "tls:[2001:db8::5]:47321")
        XCTAssertEqual(PairingRoute("tls:Box.Tail1234.ts.net:443")?.text, "tls:box.tail1234.ts.net:443")
        XCTAssertEqual(PairingRoute("tls:[2001:db8::5]:47321")?.authority, "[2001:db8::5]:47321")
        XCTAssertNil(PairingRoute("relay:anything")?.authority)
    }

    func testPrivateLANAddresses() {
        for host in ["10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.20", "fd00::1", "fc12::1", "box.local", "Box.Local."] {
            XCTAssertTrue(Pairing.isPrivateLAN(host: host), host)
        }
        for host in ["172.32.0.1", "100.64.0.1", "8.8.8.8", "2001:db8::5", "fd7a:115c:a1e0::1", "box.tail1234.ts.net", "localhost.example"] {
            XCTAssertFalse(Pairing.isPrivateLAN(host: host), host)
        }
    }

    /// What Rust's Ipv6Addr prints for each (checked with rustc); the TypeScript and Android
    /// tests carry the same list.
    func testIPv6HostsAreSpelledAsTheHubSpellsThem() {
        let spellings = [
            ("2001:DB8::5", "2001:db8::5"),
            ("2001:0db8:0000:0000:0000:0000:0000:0005", "2001:db8::5"),
            ("2001:db8:0:0:1:0:0:1", "2001:db8::1:0:0:1"),
            ("2001:db8:0:1:1:1:1:1", "2001:db8:0:1:1:1:1:1"),
            ("0:0:0:0:0:0:0:0", "::"),
            ("0:0:0:0:0:0:0:1", "::1"),
            ("::FFFF:192.168.1.20", "::ffff:192.168.1.20"),
            ("::ffff:c0a8:114", "::ffff:192.168.1.20"),
            ("::1.2.3.4", "::102:304"),
            ("fe80:0:0:0:0:0:0:1", "fe80::1"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
            ("::ffff:0:0", "::ffff:0.0.0.0"),
            ("0:0:0:0:ffff::", "::ffff:0:0:0"),
        ]
        for (input, expected) in spellings {
            XCTAssertEqual(PairingRoute("tls:[\(input)]:1"), .tls(host: expected, port: 1), input)
            XCTAssertEqual(PairingRoute("tls:[\(input)]:1")?.text, "tls:[\(expected)]:1", input)
            XCTAssertEqual(Pairing.typedAddress(input), .tls(host: expected, port: 47321), input)
        }
    }

    func testTypedAddresses() {
        XCTAssertEqual(Pairing.typedAddress("box.example"), .tls(host: "box.example", port: 47321))
        XCTAssertEqual(Pairing.typedAddress(" 192.168.1.20:8443 "), .tls(host: "192.168.1.20", port: 8443))
        XCTAssertEqual(Pairing.typedAddress("[2001:db8::5]"), .tls(host: "2001:db8::5", port: 47321))
        XCTAssertEqual(Pairing.typedAddress("[2001:db8::5]:1"), .tls(host: "2001:db8::5", port: 1))
        XCTAssertEqual(Pairing.typedAddress("2001:db8::5"), .tls(host: "2001:db8::5", port: 47321))
        for invalid in ["", "https://box.example", "box.example:0", "[2001:db8::5", "bad_host", "01.2.3.4:1"] {
            XCTAssertNil(Pairing.typedAddress(invalid), invalid)
        }
    }

    func testRouteBookMergesWithinTheLimits() {
        let day = 24 * 60 * 60 * 1000.0
        let now = 100 * day
        let saved = [SavedRoute(route: "tls:a.example:1", seen: now - 31 * day), SavedRoute(route: "tls:b.example:1", seen: now - day)]
        let merged = RouteBook.merge(saved, ["tls:B.example:1", "tls:c.example:1", "relay:x", "bad"], now: now)
        XCTAssertEqual(merged, [SavedRoute(route: "tls:b.example:1", seen: now), SavedRoute(route: "tls:c.example:1", seen: now),
                                SavedRoute(route: "relay:x", seen: now)], "expired dropped, duplicates merged, unknown kinds kept")

        let many = (0..<20).map { SavedRoute(route: "tls:h\($0).example:1", seen: now - Double(20 - $0) * 1000) }
        let capped = RouteBook.merge(many, [], now: now)
        XCTAssertEqual(capped.count, 16)
        XCTAssertEqual(capped.first?.route, "tls:h4.example:1", "the least recently seen go first")
        XCTAssertEqual(RouteBook.decode(RouteBook.encode(capped)), capped)
    }

    func testRouteExpiryCountsFromTheNewestSighting() {
        let day = 24 * 60 * 60 * 1000.0
        let now = 100 * day
        let away = now - 40 * day
        let saved = [SavedRoute(route: "tls:a.example:1", seen: away),
                     SavedRoute(route: "tls:b.example:1", seen: away - 30 * day),
                     SavedRoute(route: "tls:c.example:1", seen: away - 30 * day - 1)]
        XCTAssertEqual(RouteBook.merge(saved, [], now: now).map(\.route), ["tls:a.example:1", "tls:b.example:1"],
                       "a phone left unused for 40 days keeps what it knew")
        XCTAssertEqual(RouteBook.connected(saved, "tls:B.example:1", now: now),
                       [SavedRoute(route: "tls:a.example:1", seen: away), SavedRoute(route: "tls:b.example:1", seen: now)],
                       "a connection refreshes the winner and ages nothing else")
        XCTAssertEqual(RouteBook.connected(saved, "tls:new.example:1", now: now).map(\.route),
                       ["tls:a.example:1", "tls:b.example:1", "tls:new.example:1"])
        XCTAssertEqual(RouteBook.merge(saved, ["tls:a.example:1"], now: now).map(\.route), ["tls:a.example:1"],
                       "what the gateway reports now is the newest sighting")
    }

    func testHeadStartEndsEarlyForEveryWaiter() async {
        let headStart = HeadStart()
        let started = Date()
        async let first: Void = headStart.wait()
        async let second: Void = headStart.wait()
        headStart.end()
        _ = await (first, second)
        await headStart.wait()
        XCTAssertLessThan(Date().timeIntervalSince(started), 1, "end() lets current and later waiters go at once")
    }

    func testRaceFailuresCarryTheContractsCodes() {
        func code(_ errors: [RouteError]) -> String {
            RaceFailure(failures: errors.enumerated().map { (route: "tls:h\($0.offset).example:1", error: $0.element) }, hubName: "Box").code
        }
        XCTAssertEqual(code([.timedOut, .pinMismatch]), "PIN_MISMATCH")
        XCTAssertEqual(code([.timedOut, .timedOut]), "TIMEOUT")
        XCTAssertEqual(code([.timedOut, .unreachable("refused")]), "UNREACHABLE")
        XCTAssertEqual(code([]), "UNREACHABLE")
    }

    func testRacePlanStartsTheRememberedRouteFirst() {
        let routes = ["tls:192.168.1.20:47321", "tls:[2001:db8::5]:47321", "tls:box.tail1234.ts.net:47321", "relay:x", "tls:box.local:47321"]
        let plan = RouteBook.racePlan(routes, remembered: "tls:box.tail1234.ts.net:47321", cellular: false)
        XCTAssertEqual(plan.map(\.route.text), ["tls:box.tail1234.ts.net:47321", "tls:192.168.1.20:47321", "tls:[2001:db8::5]:47321", "tls:box.local:47321"])
        XCTAssertEqual(plan.map(\.delayed), [false, true, true, true])

        let cellular = RouteBook.racePlan(routes, remembered: "tls:192.168.1.20:47321", cellular: true)
        XCTAssertEqual(cellular.map(\.route.text), ["tls:[2001:db8::5]:47321", "tls:box.tail1234.ts.net:47321"])
        XCTAssertEqual(cellular.map(\.delayed), [false, false], "no remembered route left: all at once")

        let lanOnly = RouteBook.racePlan(["tls:192.168.1.20:47321"], remembered: nil, cellular: true)
        XCTAssertEqual(lanOnly.map(\.route.text), ["tls:192.168.1.20:47321"], "kept when it is all there is")
    }

    func testHTTPResponses() throws {
        let request = String(decoding: HTTPWire.request(method: "POST", path: "/v1/pair", authority: "[2001:db8::5]:47321", body: Data("{}".utf8)), as: UTF8.self)
        XCTAssertEqual(request, "POST /v1/pair HTTP/1.1\r\nHost: [2001:db8::5]:47321\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")

        let sized = Array("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nContent-Length: 4\r\n\r\n{}{}".utf8)
        XCTAssertNil(try HTTPWire.parseResponse(Array(sized.dropLast(1)), eof: false))
        XCTAssertThrowsError(try HTTPWire.parseResponse(Array(sized.dropLast(1)), eof: true))
        let response = try XCTUnwrap(HTTPWire.parseResponse(sized, eof: false))
        XCTAssertEqual(response.status, 200)
        XCTAssertEqual(response.headers["content-type"], "application/json")
        XCTAssertEqual(response.body, Data("{}{}".utf8))

        let chunked = Array("HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n".utf8)
        XCTAssertEqual(try HTTPWire.parseResponse(chunked, eof: false)?.body, Data("abcde".utf8))
        XCTAssertNil(try HTTPWire.parseResponse(Array(chunked.prefix(60)), eof: false))

        let unsized = Array("HTTP/1.1 200 OK\r\n\r\nall".utf8)
        XCTAssertNil(try HTTPWire.parseResponse(unsized, eof: false))
        XCTAssertEqual(try HTTPWire.parseResponse(unsized, eof: true)?.body, Data("all".utf8))

        XCTAssertThrowsError(try HTTPWire.parseResponse(Array("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n-2\r\nxx\r\n".utf8), eof: false), "a negative chunk size")
        XCTAssertThrowsError(try HTTPWire.parseResponse(Array("HTTP/1.1 200 OK\r\nContent-Length: +2\r\n\r\nxx".utf8), eof: false))
        XCTAssertThrowsError(try HTTPWire.parseHead(Array("SSH-2.0-OpenSSH\r\n\r\n".utf8)))
        XCTAssertThrowsError(try HTTPWire.parseHead([UInt8](repeating: 0x41, count: HTTPWire.maxHead + 1)))
    }

    func testWebSocketHandshake() throws {
        // RFC 6455's example.
        XCTAssertEqual(WebSocketWire.accept(for: "dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        let key = WebSocketWire.newKey()
        XCTAssertEqual(Data(base64Encoded: key)?.count, 16)
        let upgrade = String(decoding: WebSocketWire.upgradeRequest(path: "/v1/socket", authority: "h:1", key: key, headers: [("Authorization", "Bearer t")]), as: UTF8.self)
        XCTAssertTrue(upgrade.hasPrefix("GET /v1/socket HTTP/1.1\r\nHost: h:1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"))
        XCTAssertTrue(upgrade.hasSuffix("Authorization: Bearer t\r\n\r\n"))

        let answer = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: upgrade\r\nSec-WebSocket-Accept: \(WebSocketWire.accept(for: key))\r\n\r\n"
        let head = try XCTUnwrap(HTTPWire.parseHead(Array(answer.utf8)))
        XCTAssertTrue(WebSocketWire.accepts(head, key: key))
        XCTAssertFalse(WebSocketWire.accepts(head, key: WebSocketWire.newKey()))
        let refused = try XCTUnwrap(HTTPWire.parseHead(Array("HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n".utf8)))
        XCTAssertEqual(refused.status, 401)
        XCTAssertFalse(WebSocketWire.accepts(refused, key: key))
    }

    func testWebSocketFrames() throws {
        // RFC 6455 section 5.7: a masked "Hello".
        XCTAssertEqual(WebSocketWire.frame(opcode: 1, payload: Array("Hello".utf8), mask: [0x37, 0xfa, 0x21, 0x3d]),
                       [0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58])
        let long = WebSocketWire.frame(opcode: 1, payload: [UInt8](repeating: 0, count: 300), mask: [0, 0, 0, 0])
        XCTAssertEqual(Array(long.prefix(4)), [0x81, 0xfe, 0x01, 0x2c])
        XCTAssertEqual(long.count, 4 + 4 + 300)

        var reader = WebSocketReader(maxMessage: 1024)
        // A fragmented "Hel" + "lo" with a ping between, delivered a byte at a time.
        let stream: [UInt8] = [0x01, 0x03] + Array("Hel".utf8) + [0x89, 0x01, 0x2a] + [0x80, 0x02] + Array("lo".utf8)
        var events: [WebSocketReader.Event] = []
        for byte in stream { events += try reader.feed([byte]) }
        XCTAssertEqual(events, [.ping([0x2a]), .text("Hello")])

        let sized: [UInt8] = [0x81, 126, 0x01, 0x00] + [UInt8](repeating: 0x61, count: 256)
        XCTAssertEqual(try reader.feed(sized), [.text(String(repeating: "a", count: 256))])
        XCTAssertEqual(try reader.feed([0x88, 0x02, 0x03, 0xe8]), [.close(code: 1000)])

        var masked = WebSocketReader(maxMessage: 1024)
        XCTAssertThrowsError(try masked.feed([0x81, 0x81, 0, 0, 0, 0, 0x61]), "servers never mask")
        var large = WebSocketReader(maxMessage: 1024)
        XCTAssertThrowsError(try large.feed([0x81, 126, 0x04, 0x01])) { XCTAssertEqual($0 as? WebSocketReader.Failure, .tooLarge) }
        var reserved = WebSocketReader(maxMessage: 1024)
        XCTAssertThrowsError(try reserved.feed([0xc1, 0x00]), "no extension was negotiated")
        var stray = WebSocketReader(maxMessage: 1024)
        XCTAssertThrowsError(try stray.feed([0x80, 0x00]), "a continuation outside a message")
        var invalid = WebSocketReader(maxMessage: 1024)
        XCTAssertThrowsError(try invalid.feed([0x81, 0x01, 0xff]), "not UTF-8")
        var binary = WebSocketReader(maxMessage: 1024)
        XCTAssertEqual(try binary.feed([0x82, 0x01, 0x00]), [.binary])
    }
}
