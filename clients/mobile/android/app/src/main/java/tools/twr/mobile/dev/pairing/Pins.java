package tools.twr.mobile.dev.pairing;

import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.cert.Certificate;
import java.security.cert.CertificateEncodingException;

/**
 * The hub's pin: base64url without padding of SHA-256 over the leaf certificate's
 * SubjectPublicKeyInfo DER, 43 characters (docs/design/phone-pairing.md, Identity). Plain Java:
 * no Android, Capacitor or OkHttp types, so the JVM unit tests run it as the app does.
 */
public final class Pins {
    private Pins() {}

    /** Bytes in a raw pin, the SHA-256. */
    public static final int PIN_BYTES = 32;

    public static byte[] sha256(byte[] data) {
        try {
            return MessageDigest.getInstance("SHA-256").digest(data);
        } catch (NoSuchAlgorithmException impossible) {
            throw new IllegalStateException("SHA-256 is unavailable", impossible);
        }
    }

    /** The pin of a SubjectPublicKeyInfo DER. */
    public static String pin(byte[] spkiDer) {
        return Base64Url.encode(sha256(spkiDer));
    }

    /** The raw pin of a certificate, from the SubjectPublicKeyInfo bytes exactly as it carries them. */
    public static byte[] pinBytes(Certificate certificate) throws CertificateEncodingException {
        return sha256(spki(certificate.getEncoded()));
    }

    public static String pin(Certificate certificate) throws CertificateEncodingException {
        return Base64Url.encode(pinBytes(certificate));
    }

    /** A pin string's 32 bytes; refuses anything else. */
    public static byte[] decodePin(String pin) {
        if (pin == null || pin.length() != 43) throw new IllegalArgumentException("a pin is 43 base64url characters");
        byte[] raw = Base64Url.decode(pin);
        if (raw.length != PIN_BYTES) throw new IllegalArgumentException("a pin is 32 bytes");
        return raw;
    }

    /** Whether {@code pin} is a well-formed pin string. */
    public static boolean valid(String pin) {
        try {
            decodePin(pin);
            return true;
        } catch (IllegalArgumentException bad) {
            return false;
        }
    }

    /** Compares in time that depends only on the lengths. */
    public static boolean equal(byte[] a, byte[] b) {
        if (a == null || b == null || a.length != b.length) return false;
        int difference = 0;
        for (int i = 0; i < a.length; i++) difference |= a[i] ^ b[i];
        return difference == 0;
    }

    /**
     * The SubjectPublicKeyInfo element of an X.509 certificate's DER, byte for byte. Taken from
     * the encoding rather than {@code getPublicKey().getEncoded()}, which a provider may
     * re-encode (a compressed point, say) and so hash differently from the gateway.
     */
    public static byte[] spki(byte[] certificateDer) throws CertificateEncodingException {
        try {
            Der certificate = Der.read(certificateDer, 0, certificateDer.length, 0x30);
            Der tbs = Der.read(certificateDer, certificate.contentStart, certificate.end, 0x30);
            int at = tbs.contentStart;
            Der field = Der.read(certificateDer, at, tbs.end, -1);
            if (field.tag == 0xa0) field = Der.read(certificateDer, field.end, tbs.end, -1); // [0] version
            // serial, signature algorithm, issuer, validity, subject, then the key.
            for (int skipped = 0; skipped < 5; skipped++) field = Der.read(certificateDer, field.end, tbs.end, -1);
            if (field.tag != 0x30) throw new CertificateEncodingException("no SubjectPublicKeyInfo");
            byte[] spki = new byte[field.end - field.start];
            System.arraycopy(certificateDer, field.start, spki, 0, spki.length);
            return spki;
        } catch (IllegalArgumentException bad) {
            throw new CertificateEncodingException("malformed certificate: " + bad.getMessage());
        }
    }

    /** One DER element: its tag, where it starts, where its contents start, and where it ends. */
    private static final class Der {
        final int tag, start, contentStart, end;

        private Der(int tag, int start, int contentStart, int end) {
            this.tag = tag;
            this.start = start;
            this.contentStart = contentStart;
            this.end = end;
        }

        /** The element at {@code at}, within {@code limit}; {@code expect} is its tag, or -1 for any. */
        static Der read(byte[] der, int at, int limit, int expect) {
            if (at + 2 > limit) throw new IllegalArgumentException("truncated");
            int tag = der[at] & 0xff;
            if ((tag & 0x1f) == 0x1f) throw new IllegalArgumentException("multi-byte tag");
            if (expect >= 0 && tag != expect) throw new IllegalArgumentException("unexpected tag");
            int first = der[at + 1] & 0xff;
            int position = at + 2;
            long length;
            if (first < 0x80) {
                length = first;
            } else {
                int count = first & 0x7f;
                if (count == 0 || count > 4 || position + count > limit) throw new IllegalArgumentException("bad length");
                length = 0;
                for (int i = 0; i < count; i++) length = (length << 8) | (der[position++] & 0xff);
            }
            if (length > limit - position) throw new IllegalArgumentException("truncated");
            return new Der(tag, at, position, position + (int) length);
        }
    }
}
