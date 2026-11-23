package tools.twr.mobile.dev.pairing;

import java.io.File;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * The shared test vectors, clients/shared/fixtures/phone-pairing-v2.json, which the hub's
 * {@code cargo test -p twr-core phone_pairing} writes. A small JSON reader, because Android's
 * org.json is a stub in local unit tests.
 */
final class Fixture {
    private Fixture() {}

    private static Map<String, Object> vectors;

    static synchronized Map<String, Object> vectors() throws IOException {
        if (vectors == null) vectors = object(new Reader(new String(Files.readAllBytes(file().toPath()), StandardCharsets.UTF_8)).value());
        return vectors;
    }

    /** {@code -Dtwr.pairingFixture=…}, else found by walking up from the working directory. */
    static File file() throws IOException {
        String configured = System.getProperty("twr.pairingFixture");
        if (configured != null) return new File(configured);
        for (File dir = new File("").getAbsoluteFile(); dir != null; dir = dir.getParentFile()) {
            File candidate = new File(dir, "clients/shared/fixtures/phone-pairing-v2.json");
            if (candidate.isFile()) return candidate;
            candidate = new File(dir, "shared/fixtures/phone-pairing-v2.json");
            if (candidate.isFile()) return candidate;
        }
        throw new IOException("phone-pairing-v2.json not found above " + new File("").getAbsolutePath());
    }

    static String string(Map<String, Object> map, String key) {
        return (String) map.get(key);
    }

    @SuppressWarnings("unchecked")
    static Map<String, Object> object(Object value) {
        return (Map<String, Object>) value;
    }

    @SuppressWarnings("unchecked")
    static List<Object> array(Object value) {
        return (List<Object>) value;
    }

    static byte[] hex(String text) {
        byte[] out = new byte[text.length() / 2];
        for (int i = 0; i < out.length; i++) out[i] = (byte) Integer.parseInt(text.substring(2 * i, 2 * i + 2), 16);
        return out;
    }

    static String hex(byte[] bytes) {
        StringBuilder out = new StringBuilder();
        for (byte b : bytes) out.append(String.format("%02x", b & 0xff));
        return out.toString();
    }

    static Object parse(String json) {
        return new Reader(json).value();
    }

    private static final class Reader {
        private final String text;
        private int at;

        Reader(String text) {
            this.text = text;
        }

        Object value() {
            space();
            char c = text.charAt(at);
            if (c == '{') {
                at++;
                Map<String, Object> map = new LinkedHashMap<>();
                space();
                if (text.charAt(at) == '}') {
                    at++;
                    return map;
                }
                while (true) {
                    space();
                    String key = string();
                    space();
                    expect(':');
                    map.put(key, value());
                    space();
                    if (text.charAt(at) == ',') {
                        at++;
                        continue;
                    }
                    expect('}');
                    return map;
                }
            }
            if (c == '[') {
                at++;
                List<Object> list = new ArrayList<>();
                space();
                if (text.charAt(at) == ']') {
                    at++;
                    return list;
                }
                while (true) {
                    list.add(value());
                    space();
                    if (text.charAt(at) == ',') {
                        at++;
                        continue;
                    }
                    expect(']');
                    return list;
                }
            }
            if (c == '"') return string();
            if (text.startsWith("true", at)) {
                at += 4;
                return Boolean.TRUE;
            }
            if (text.startsWith("false", at)) {
                at += 5;
                return Boolean.FALSE;
            }
            if (text.startsWith("null", at)) {
                at += 4;
                return null;
            }
            int start = at;
            while (at < text.length() && "+-.eE0123456789".indexOf(text.charAt(at)) >= 0) at++;
            String number = text.substring(start, at);
            if (number.isEmpty()) throw new IllegalArgumentException("bad JSON at " + start);
            return number.matches("-?\\d+") ? (Object) Long.parseLong(number) : (Object) Double.parseDouble(number);
        }

        private String string() {
            expect('"');
            StringBuilder out = new StringBuilder();
            while (true) {
                char c = text.charAt(at++);
                if (c == '"') return out.toString();
                if (c != '\\') {
                    out.append(c);
                    continue;
                }
                char e = text.charAt(at++);
                switch (e) {
                    case 'n': out.append('\n'); break;
                    case 't': out.append('\t'); break;
                    case 'r': out.append('\r'); break;
                    case 'b': out.append('\b'); break;
                    case 'f': out.append('\f'); break;
                    case 'u': out.append((char) Integer.parseInt(text.substring(at, at + 4), 16)); at += 4; break;
                    default: out.append(e);
                }
            }
        }

        private void space() {
            while (at < text.length() && Character.isWhitespace(text.charAt(at))) at++;
        }

        private void expect(char c) {
            if (text.charAt(at) != c) throw new IllegalArgumentException("expected " + c + " at " + at);
            at++;
        }
    }
}
