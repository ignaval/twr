package tools.twr.mobile.dev.pairing;

import java.util.Locale;

/**
 * One route to a pinned hub, {@code <kind>:<rest>} (docs/design/phone-pairing.md, Routes).
 * {@code tls:<host>:<port>} is dialed; any other lowercase kind ({@code relay:} in phase 1) is
 * kept and never dialed. Parsing matches the hub's {@code Route::parse} and never touches DNS.
 */
public final class Route {
    public static final int DEFAULT_PORT = 47321;

    /** {@code "tls"}, or the unknown kind's name. */
    public final String kind;
    /** For {@code tls}: an IPv4 literal, an IPv6 literal without brackets, or a lowercase DNS name. */
    public final String host;
    public final int port;
    private final String text;

    private Route(String kind, String host, int port, String text) {
        this.kind = kind;
        this.host = host;
        this.port = port;
        this.text = text;
    }

    /** Parses a route; throws {@link IllegalArgumentException} with the reason. */
    public static Route parse(String text) {
        if (text == null || text.isEmpty() || text.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > 255) throw new IllegalArgumentException("bad route");
        int colon = text.indexOf(':');
        if (colon < 0) throw new IllegalArgumentException("a route is <kind>:<rest>");
        String kind = text.substring(0, colon);
        if (kind.isEmpty()) throw new IllegalArgumentException("bad route kind");
        for (int i = 0; i < kind.length(); i++) {
            char c = kind.charAt(i);
            if (c < 'a' || c > 'z') throw new IllegalArgumentException("bad route kind");
        }
        if (!kind.equals("tls")) return new Route(kind, null, 0, text);
        String rest = text.substring(colon + 1);
        int last = rest.lastIndexOf(':');
        if (last < 0) throw new IllegalArgumentException("tls route needs a port");
        return tls(rest.substring(0, last), port(rest.substring(last + 1)));
    }

    /**
     * A typed address: {@code host}, {@code host:port}, {@code [IPv6]}, {@code [IPv6]:port} or a
     * bare IPv6 literal. The port defaults to {@link #DEFAULT_PORT}.
     */
    public static Route fromAddress(String address) {
        String input = address == null ? "" : address.trim();
        if (input.isEmpty()) throw new IllegalArgumentException("enter the hub's address");
        if (input.startsWith("[")) {
            int close = input.indexOf(']');
            if (close < 0) throw new IllegalArgumentException("bad IPv6 literal");
            String after = input.substring(close + 1);
            if (after.isEmpty()) return tls(input.substring(0, close + 1), DEFAULT_PORT);
            if (!after.startsWith(":")) throw new IllegalArgumentException("bad address");
            return tls(input.substring(0, close + 1), port(after.substring(1)));
        }
        int first = input.indexOf(':');
        if (first < 0) return tls(input, DEFAULT_PORT);
        if (input.indexOf(':', first + 1) >= 0) return tls("[" + input + "]", DEFAULT_PORT); // bare IPv6
        return tls(input.substring(0, first), port(input.substring(first + 1)));
    }

    private static Route tls(String host, int port) {
        String normalized;
        if (host.startsWith("[")) {
            if (!host.endsWith("]")) throw new IllegalArgumentException("bad IPv6 literal");
            String inner = host.substring(1, host.length() - 1);
            if (ipv6(inner) == null) throw new IllegalArgumentException("bad IPv6 literal");
            normalized = inner.toLowerCase(Locale.ROOT);
        } else if (ipv4(host) != null) {
            normalized = host;
        } else if (validDnsName(host)) {
            normalized = host.toLowerCase(Locale.ROOT);
        } else {
            throw new IllegalArgumentException("bad host");
        }
        String printed = normalized.indexOf(':') >= 0 ? "tls:[" + normalized + "]:" + port : "tls:" + normalized + ":" + port;
        return new Route("tls", normalized, port, printed);
    }

    private static int port(String text) {
        if (text.isEmpty() || text.length() > 5) throw new IllegalArgumentException("bad port");
        int port = 0;
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c < '0' || c > '9') throw new IllegalArgumentException("bad port");
            port = port * 10 + (c - '0');
        }
        if (port < 1 || port > 65535) throw new IllegalArgumentException("bad port");
        return port;
    }

    /** Whether the app dials this route (only {@code tls} in phase 1). */
    public boolean dialable() {
        return kind.equals("tls");
    }

    /** {@code https://host:port}, IPv6 in brackets: the display URL and the base of requests. */
    public String origin() {
        if (!dialable()) throw new IllegalStateException("not a tls route");
        return "https://" + (host.indexOf(':') >= 0 ? "[" + host + "]" : host) + ":" + port;
    }

    /**
     * A private LAN address, skipped on cellular: RFC 1918, fc00::/7 outside the tailnet's
     * fd7a:115c:a1e0::/48, and {@code .local} names.
     */
    public boolean privateLan() {
        if (!dialable()) return false;
        int[] v4 = ipv4(host);
        if (v4 == null && host.indexOf(':') >= 0) {
            int[] v6 = ipv6(host);
            if (v6 == null) return false;
            if (mappedIpv4(v6)) {
                v4 = new int[] {v6[12], v6[13], v6[14], v6[15]};
            } else {
                boolean uniqueLocal = (v6[0] & 0xfe) == 0xfc;
                boolean tailnet = v6[0] == 0xfd && v6[1] == 0x7a && v6[2] == 0x11 && v6[3] == 0x5c && v6[4] == 0xa1 && v6[5] == 0xe0;
                return uniqueLocal && !tailnet;
            }
        }
        if (v4 != null) {
            return v4[0] == 10 || (v4[0] == 172 && (v4[1] & 0xf0) == 16) || (v4[0] == 192 && v4[1] == 168);
        }
        return host.equals("local") || host.endsWith(".local");
    }

    private static boolean mappedIpv4(int[] v6) {
        for (int i = 0; i < 10; i++) if (v6[i] != 0) return false;
        return v6[10] == 0xff && v6[11] == 0xff;
    }

    /** The canonical form: what is stored and deduplicated. */
    @Override
    public String toString() {
        return text;
    }

    @Override
    public boolean equals(Object other) {
        return other instanceof Route && ((Route) other).text.equals(text);
    }

    @Override
    public int hashCode() {
        return text.hashCode();
    }

    /** A strict dotted-quad (no leading zeros, as Rust's {@code Ipv4Addr}), or null. */
    static int[] ipv4(String text) {
        String[] parts = text.split("\\.", -1);
        if (parts.length != 4) return null;
        int[] octets = new int[4];
        for (int i = 0; i < 4; i++) {
            String part = parts[i];
            if (part.isEmpty() || part.length() > 3 || (part.length() > 1 && part.charAt(0) == '0')) return null;
            int value = 0;
            for (int j = 0; j < part.length(); j++) {
                char c = part.charAt(j);
                if (c < '0' || c > '9') return null;
                value = value * 10 + (c - '0');
            }
            if (value > 255) return null;
            octets[i] = value;
        }
        return octets;
    }

    /** An IPv6 literal (with {@code ::} and an optional trailing dotted quad) as 16 bytes, or null. */
    static int[] ipv6(String text) {
        if (text.isEmpty() || text.length() > 45) return null;
        int[] groups = new int[8];
        int count = 0, gap = -1, i = 0, n = text.length();
        if (text.startsWith("::")) {
            gap = 0;
            i = 2;
            if (i == n) return expand(groups, count, gap);
        } else if (text.startsWith(":")) {
            return null;
        }
        while (i < n) {
            if (count == 8) return null;
            int end = i;
            while (end < n && text.charAt(end) != ':') end++;
            String part = text.substring(i, end);
            if (part.indexOf('.') >= 0) {
                if (end != n || count > 6) return null;
                int[] v4 = ipv4(part);
                if (v4 == null) return null;
                groups[count++] = (v4[0] << 8) | v4[1];
                groups[count++] = (v4[2] << 8) | v4[3];
                break;
            }
            if (part.isEmpty() || part.length() > 4) return null;
            int value = 0;
            for (int j = 0; j < part.length(); j++) {
                int digit = Character.digit(part.charAt(j), 16);
                if (digit < 0 || part.charAt(j) > 'f') return null;
                value = (value << 4) | digit;
            }
            groups[count++] = value;
            if (end == n) break;
            // At a ':'.
            if (end + 1 < n && text.charAt(end + 1) == ':') {
                if (gap >= 0) return null;
                gap = count;
                i = end + 2;
                if (i == n) break;
            } else {
                i = end + 1;
                if (i == n) return null; // trailing single ':'
            }
        }
        return expand(groups, count, gap);
    }

    private static int[] expand(int[] groups, int count, int gap) {
        if (gap < 0 && count != 8) return null;
        if (gap >= 0 && count > 7) return null;
        int[] full = new int[8];
        if (gap < 0) {
            System.arraycopy(groups, 0, full, 0, 8);
        } else {
            System.arraycopy(groups, 0, full, 0, gap);
            int tail = count - gap;
            System.arraycopy(groups, gap, full, 8 - tail, tail);
        }
        int[] bytes = new int[16];
        for (int g = 0; g < 8; g++) {
            bytes[2 * g] = full[g] >> 8;
            bytes[2 * g + 1] = full[g] & 0xff;
        }
        return bytes;
    }

    private static boolean validDnsName(String name) {
        if (name.isEmpty() || name.length() > 253) return false;
        boolean letter = false;
        for (String label : name.split("\\.", -1)) {
            if (label.isEmpty() || label.length() > 63 || label.startsWith("-") || label.endsWith("-")) return false;
            for (int i = 0; i < label.length(); i++) {
                char c = label.charAt(i);
                boolean alpha = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
                if (!alpha && !(c >= '0' && c <= '9') && c != '-') return false;
                letter |= alpha;
            }
        }
        return letter;
    }
}
