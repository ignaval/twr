package tools.twr.mobile.dev.pairing;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import org.junit.Test;

/** Every vector in clients/shared/fixtures/phone-pairing-v2.json that the Android side computes. */
public class PairingVectorsTest {
    private static Map<String, Object> v() throws Exception {
        return Fixture.vectors();
    }

    @Test
    public void pinIsBase64urlSha256OfTheSpki() throws Exception {
        byte[] spki = Fixture.hex(Fixture.string(v(), "spki_hex"));
        assertEquals(Fixture.string(v(), "pin"), Pins.pin(spki));
        assertEquals(43, Pins.pin(spki).length());
        assertArrayEquals(Pins.sha256(spki), Pins.decodePin(Fixture.string(v(), "pin")));
    }

    @Test
    public void codesNormalizeAsTheHubDoes() throws Exception {
        Map<String, Object> codes = Fixture.object(v().get("codes"));
        for (Object pair : Fixture.array(codes.get("valid"))) {
            List<Object> io = Fixture.array(pair);
            assertEquals((String) io.get(0), io.get(1), PairingProof.normalizeCode((String) io.get(0)));
        }
        for (Object bad : Fixture.array(codes.get("invalid"))) {
            assertNull((String) bad, PairingProof.normalizeCode((String) bad));
        }
        assertEquals(Fixture.string(v(), "code"), PairingProof.normalizeCode(Fixture.string(v(), "code")));
    }

    /** The first check of the hub's Argon2id output by an independent implementation (Bouncy Castle). */
    @Test
    public void argon2idStretchMatchesTheHub() throws Exception {
        Map<String, Object> argon = Fixture.object(v().get("argon2id"));
        assertEquals(65536L, argon.get("m_kib"));
        assertEquals(3L, argon.get("t"));
        assertEquals(1L, argon.get("p"));
        assertEquals(32L, argon.get("len"));
        assertEquals("twr-mobile-pair-v2", argon.get("salt"));
        byte[] stretched = PairingProof.stretch(Fixture.string(v(), "code"));
        assertEquals(Fixture.string(v(), "stretched_hex"), Fixture.hex(stretched));
        // Separators and case don't change it: the code is normalized first.
        assertArrayEquals(stretched, PairingProof.stretch("abcd-efgh-jk23"));
    }

    @Test
    public void hkdfProofKeyMatchesTheHub() throws Exception {
        Map<String, Object> hkdf = Fixture.object(v().get("hkdf"));
        assertEquals("twr-mobile-pair-v2", hkdf.get("salt"));
        assertEquals("pin-proof", hkdf.get("info"));
        // HKDF alone, from the stretched vector, then the whole derivation from the code.
        byte[] fromStretched = PairingProof.hkdfSha256(
                "twr-mobile-pair-v2".getBytes(StandardCharsets.US_ASCII),
                Fixture.hex(Fixture.string(v(), "stretched_hex")),
                "pin-proof".getBytes(StandardCharsets.US_ASCII), 32);
        assertEquals(Fixture.string(v(), "proof_key_hex"), Fixture.hex(fromStretched));
        assertEquals(Fixture.string(v(), "proof_key_hex"), Fixture.hex(PairingProof.proofKey(Fixture.string(v(), "code"))));
    }

    @Test
    public void hkdfMatchesRfc5869CaseOne() {
        byte[] okm = PairingProof.hkdfSha256(Fixture.hex("000102030405060708090a0b0c"),
                Fixture.hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b"), Fixture.hex("f0f1f2f3f4f5f6f7f8f9"), 42);
        assertEquals("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865", Fixture.hex(okm));
    }

    @Test
    public void proofMatchesAndVerifiesOnlyForItsInputs() throws Exception {
        byte[] key = Fixture.hex(Fixture.string(v(), "proof_key_hex"));
        byte[] pin = Pins.decodePin(Fixture.string(v(), "pin"));
        byte[] nonce = Base64Url.decode(Fixture.string(v(), "nonce_b64url"));
        String gateway = Fixture.string(v(), "gateway_id");
        byte[] proof = Base64Url.decode(Fixture.string(v(), "proof_b64url"));
        assertEquals(32, nonce.length);
        for (int i = 0; i < 32; i++) assertEquals(i, nonce[i]);
        assertEquals(Fixture.string(v(), "proof_b64url"), Base64Url.encode(PairingProof.proof(key, pin, nonce, gateway)));
        assertTrue(PairingProof.verify(key, pin, nonce, gateway, proof));

        byte[] otherPin = pin.clone();
        otherPin[0] ^= 1;
        assertFalse("another key", PairingProof.verify(key, otherPin, nonce, gateway, proof));
        byte[] otherNonce = nonce.clone();
        otherNonce[31] ^= 1;
        assertFalse("another nonce", PairingProof.verify(key, pin, otherNonce, gateway, proof));
        assertFalse("another gateway", PairingProof.verify(key, pin, nonce, gateway + "x", proof));
        byte[] otherKey = key.clone();
        otherKey[5] ^= 1;
        assertFalse("another code", PairingProof.verify(otherKey, pin, nonce, gateway, proof));
        byte[] flipped = proof.clone();
        flipped[17] ^= 0x40;
        assertFalse("a flipped bit", PairingProof.verify(key, pin, nonce, gateway, flipped));
        assertFalse("truncated", PairingProof.verify(key, pin, nonce, gateway, java.util.Arrays.copyOf(proof, 31)));
        assertFalse("short nonce", PairingProof.verify(key, pin, new byte[16], gateway, proof));
    }

    @Test
    public void invitationLinkIsBase64urlOfTheJson() throws Exception {
        String link = Fixture.string(v(), "invitation_link");
        assertTrue(link.startsWith("twr://pair#"));
        String json = new String(Base64Url.decode(link.substring("twr://pair#".length())), StandardCharsets.UTF_8);
        assertEquals(Fixture.string(v(), "invitation_json"), json);
        Map<String, Object> invitation = Fixture.object(Fixture.parse(json));
        assertEquals(Fixture.string(v(), "pin"), invitation.get("pin"));
        assertTrue(Pins.valid((String) invitation.get("pin")));
        for (Object route : Fixture.array(invitation.get("routes"))) Route.parse((String) route);
    }

    @Test
    public void routesParseAsTheHubDoes() throws Exception {
        Map<String, Object> routes = Fixture.object(v().get("routes"));
        for (Object pair : Fixture.array(routes.get("valid"))) {
            List<Object> io = Fixture.array(pair);
            String input = (String) io.get(0);
            Map<String, Object> expected = Fixture.object(io.get(1));
            Route route = Route.parse(input);
            if ("other".equals(expected.get("kind"))) {
                assertFalse(input, route.dialable());
                assertEquals(input, route.toString());
            } else {
                assertTrue(input, route.dialable());
                assertEquals(input, expected.get("kind"), route.kind);
                assertEquals(input, expected.get("host"), route.host);
                assertEquals(input, ((Long) expected.get("port")).intValue(), route.port);
            }
        }
        for (Object bad : Fixture.array(routes.get("invalid"))) {
            try {
                Route.parse((String) bad);
                fail("accepted " + bad);
            } catch (IllegalArgumentException expected) {
                // refused
            }
        }
    }

    @Test
    public void base64urlIsStrict() {
        for (int len = 0; len < 40; len++) {
            byte[] bytes = new byte[len];
            for (int i = 0; i < len; i++) bytes[i] = (byte) (i * 37 + 11);
            assertArrayEquals(bytes, Base64Url.decode(Base64Url.encode(bytes)));
        }
        assertEquals("-_8", Base64Url.encode(new byte[] {(byte) 0xfb, (byte) 0xff}));
        for (String bad : new String[] {"AA==", "A+", "AB", "A", "AA A"}) {
            try {
                Base64Url.decode(bad);
                fail("accepted " + bad);
            } catch (IllegalArgumentException expected) {
                // refused
            }
        }
        assertFalse(Pins.valid("short"));
        assertFalse(Pins.valid(null));
    }
}
