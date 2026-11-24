package tools.twr.mobile.dev.pairing;

import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.util.Arrays;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;
import org.bouncycastle.crypto.generators.Argon2BytesGenerator;
import org.bouncycastle.crypto.params.Argon2Parameters;

/**
 * Pairing codes and the typed-code pin proof (docs/design/phone-pairing.md, The proof key and
 * the pin proof):
 *
 * <pre>
 * stretched = Argon2id(normalized code, salt "twr-mobile-pair-v2", 64 MiB, t 3, p 1, v 0x13, 32 bytes)
 * K_proof   = HKDF-SHA256(salt "twr-mobile-pair-v2", ikm stretched, info "pin-proof", 32 bytes)
 * proof     = HMAC-SHA256(K_proof, "twr-mobile-pin-v2" 0x00 || pin (32) || nonce (32) || gateway_id)
 * </pre>
 *
 * Argon2id takes 64 MiB and about a second on a phone: never call it on the main thread.
 */
public final class PairingProof {
    private PairingProof() {}

    public static final String ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    public static final int CODE_LENGTH = 12;
    public static final int NONCE_BYTES = 32;
    private static final byte[] SALT = "twr-mobile-pair-v2".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] INFO = "pin-proof".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] LABEL = "twr-mobile-pin-v2\0".getBytes(StandardCharsets.US_ASCII);

    /** Strips ASCII spaces and {@code -}, uppercases ASCII; the code if exactly 12 alphabet symbols, else null. */
    public static String normalizeCode(String input) {
        if (input == null) return null;
        StringBuilder code = new StringBuilder(CODE_LENGTH);
        for (int i = 0; i < input.length(); i++) {
            char c = input.charAt(i);
            if (c == ' ' || c == '-') continue;
            if (c >= 'a' && c <= 'z') c = (char) (c - 'a' + 'A');
            if (ALPHABET.indexOf(c) < 0 || code.length() == CODE_LENGTH) return null;
            code.append(c);
        }
        return code.length() == CODE_LENGTH ? code.toString() : null;
    }

    /** The Argon2id step; {@code code} in any form {@link #normalizeCode} accepts. */
    public static byte[] stretch(String code) {
        String normalized = normalizeCode(code);
        if (normalized == null) throw new IllegalArgumentException("not a pairing code");
        Argon2Parameters parameters = new Argon2Parameters.Builder(Argon2Parameters.ARGON2_id)
                .withVersion(Argon2Parameters.ARGON2_VERSION_13)
                .withMemoryAsKB(65536)
                .withIterations(3)
                .withParallelism(1)
                .withSalt(SALT)
                .build();
        Argon2BytesGenerator generator = new Argon2BytesGenerator();
        generator.init(parameters);
        byte[] stretched = new byte[32];
        byte[] password = normalized.getBytes(StandardCharsets.US_ASCII);
        generator.generateBytes(password, stretched);
        Arrays.fill(password, (byte) 0);
        return stretched;
    }

    /** {@code K_proof} for a code. */
    public static byte[] proofKey(String code) {
        byte[] stretched = stretch(code);
        try {
            return hkdfSha256(SALT, stretched, INFO, 32);
        } finally {
            Arrays.fill(stretched, (byte) 0);
        }
    }

    /** RFC 5869 HKDF with SHA-256. */
    public static byte[] hkdfSha256(byte[] salt, byte[] ikm, byte[] info, int length) {
        if (length < 1 || length > 255 * 32) throw new IllegalArgumentException("bad HKDF length");
        byte[] prk = hmac(salt, ikm);
        byte[] out = new byte[length];
        byte[] block = new byte[0];
        int written = 0;
        for (int counter = 1; written < length; counter++) {
            byte[] input = new byte[block.length + info.length + 1];
            System.arraycopy(block, 0, input, 0, block.length);
            System.arraycopy(info, 0, input, block.length, info.length);
            input[input.length - 1] = (byte) counter;
            block = hmac(prk, input);
            int take = Math.min(block.length, length - written);
            System.arraycopy(block, 0, out, written, take);
            written += take;
        }
        Arrays.fill(prk, (byte) 0);
        return out;
    }

    /** The proof for a raw pin (32 bytes), a nonce (32 bytes) and the gateway id /health served. */
    public static byte[] proof(byte[] key, byte[] pin, byte[] nonce, String gatewayId) {
        if (key.length != 32) throw new IllegalArgumentException("a proof key is 32 bytes");
        if (pin.length != Pins.PIN_BYTES) throw new IllegalArgumentException("a pin is 32 bytes");
        if (nonce.length != NONCE_BYTES) throw new IllegalArgumentException("a nonce is 32 bytes");
        byte[] gateway = gatewayId.getBytes(StandardCharsets.UTF_8);
        byte[] message = new byte[LABEL.length + pin.length + nonce.length + gateway.length];
        int at = 0;
        System.arraycopy(LABEL, 0, message, at, LABEL.length);
        at += LABEL.length;
        System.arraycopy(pin, 0, message, at, pin.length);
        at += pin.length;
        System.arraycopy(nonce, 0, message, at, nonce.length);
        at += nonce.length;
        System.arraycopy(gateway, 0, message, at, gateway.length);
        return hmac(key, message);
    }

    /** Checks a proof the gateway sent, in constant time. False for any malformed input. */
    public static boolean verify(byte[] key, byte[] pin, byte[] nonce, String gatewayId, byte[] proof) {
        if (key == null || pin == null || nonce == null || gatewayId == null || proof == null) return false;
        byte[] expected;
        try {
            expected = proof(key, pin, nonce, gatewayId);
        } catch (IllegalArgumentException malformed) {
            return false;
        }
        return Pins.equal(expected, proof);
    }

    private static byte[] hmac(byte[] key, byte[] data) {
        try {
            Mac mac = Mac.getInstance("HmacSHA256");
            // An empty key is legal HMAC but SecretKeySpec refuses it; HMAC pads it to zeros anyway.
            mac.init(new SecretKeySpec(key.length == 0 ? new byte[32] : key, "HmacSHA256"));
            return mac.doFinal(data);
        } catch (GeneralSecurityException impossible) {
            throw new IllegalStateException("HMAC-SHA256 is unavailable", impossible);
        }
    }
}
