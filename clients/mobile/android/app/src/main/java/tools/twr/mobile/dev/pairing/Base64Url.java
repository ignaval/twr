package tools.twr.mobile.dev.pairing;

/**
 * base64url without padding, strict as the hub's: padding, whitespace, the other alphabet and
 * nonzero trailing bits are refused. (java.util.Base64 needs API 26; the app supports 24.)
 */
public final class Base64Url {
    private Base64Url() {}

    private static final char[] ALPHABET =
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_".toCharArray();

    public static String encode(byte[] bytes) {
        StringBuilder out = new StringBuilder((bytes.length * 4 + 2) / 3);
        for (int i = 0; i < bytes.length; i += 3) {
            int remaining = Math.min(3, bytes.length - i);
            int n = (bytes[i] & 0xff) << 16;
            if (remaining > 1) n |= (bytes[i + 1] & 0xff) << 8;
            if (remaining > 2) n |= bytes[i + 2] & 0xff;
            for (int j = 0; j <= remaining; j++) out.append(ALPHABET[(n >> (18 - 6 * j)) & 63]);
        }
        return out.toString();
    }

    public static byte[] decode(String text) {
        if (text.length() % 4 == 1) throw new IllegalArgumentException("bad base64url length");
        byte[] out = new byte[text.length() * 3 / 4];
        int written = 0, accumulator = 0, bits = 0;
        for (int i = 0; i < text.length(); i++) {
            int value = value(text.charAt(i));
            if (value < 0) throw new IllegalArgumentException("bad base64url character");
            accumulator = (accumulator << 6) | value;
            bits += 6;
            if (bits >= 8) {
                bits -= 8;
                out[written++] = (byte) (accumulator >> bits);
                accumulator &= (1 << bits) - 1;
            }
        }
        if (accumulator != 0) throw new IllegalArgumentException("bad base64url padding bits");
        if (written == out.length) return out;
        byte[] exact = new byte[written];
        System.arraycopy(out, 0, exact, 0, written);
        return exact;
    }

    private static int value(char c) {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        if (c == '-') return 62;
        if (c == '_') return 63;
        return -1;
    }
}
