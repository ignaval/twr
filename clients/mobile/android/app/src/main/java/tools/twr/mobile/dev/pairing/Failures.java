package tools.twr.mobile.dev.pairing;

import java.io.InterruptedIOException;
import java.net.ConnectException;
import java.net.NoRouteToHostException;
import java.net.PortUnreachableException;
import java.net.SocketTimeoutException;
import java.net.UnknownHostException;
import java.util.List;
import javax.net.ssl.SSLException;
import javax.net.ssl.SSLPeerUnverifiedException;

/**
 * Error messages that name the route and the reason (docs/design/phone-pairing.md, Errors):
 * unreachable, timed out, another device's key ({@code PIN_MISMATCH}), or bad code.
 */
public final class Failures {
    private Failures() {}

    public static final String PIN_MISMATCH = "PIN_MISMATCH";
    public static final String TIMEOUT = "TIMEOUT";
    public static final String UNREACHABLE = "UNREACHABLE";
    public static final String BAD_CODE = "BAD_CODE";

    /** Whether the server presented a key other than the pin, anywhere in the cause chain. */
    public static boolean pinMismatch(Throwable error) {
        for (Throwable cause = error; cause != null; cause = cause.getCause()) {
            if (cause instanceof PinMismatchException || cause instanceof SSLPeerUnverifiedException) return true;
            String message = cause.getMessage();
            if (message != null && message.contains(PIN_MISMATCH)) return true;
            if (cause.getCause() == cause) break;
        }
        return false;
    }

    public static boolean timedOut(Throwable error) {
        for (Throwable cause = error; cause != null; cause = cause.getCause()) {
            if (cause instanceof SocketTimeoutException || cause instanceof RouteRace.DeadlineExceeded) return true;
            if (cause instanceof InterruptedIOException && "timeout".equals(cause.getMessage())) return true;
            if (cause.getCause() == cause) break;
        }
        return false;
    }

    /** One route's reason, for a person. {@code hub} is the hub's display name. */
    public static String reason(Throwable error, String hub) {
        if (pinMismatch(error)) return PIN_MISMATCH + ": this is another device, not " + hub;
        if (timedOut(error)) return "timed out";
        for (Throwable cause = error; cause != null; cause = cause.getCause()) {
            if (cause instanceof HubMismatchException) return cause.getMessage();
            if (cause instanceof UnknownHostException) return "unknown host";
            if (cause instanceof ConnectException || cause instanceof NoRouteToHostException || cause instanceof PortUnreachableException) {
                return "unreachable";
            }
            if (cause instanceof SSLException) return "TLS failed (" + clip(cause.getMessage()) + ")";
            if (cause.getCause() == cause) break;
        }
        String message = error.getMessage();
        return message == null || message.isEmpty() ? "unreachable (" + error.getClass().getSimpleName() + ")" : clip(message);
    }

    /** {@code "tls:a:1 timed out; tls:b:2 PIN_MISMATCH: …"}, every failed route in order. */
    public static String summary(List<RouteRace.Failure> failures, List<Route> skipped, String hub) {
        StringBuilder out = new StringBuilder();
        for (RouteRace.Failure failure : failures) {
            if (out.length() > 0) out.append("; ");
            out.append(failure.route).append(' ').append(reason(failure.error, hub));
        }
        if (failures.isEmpty()) {
            out.append(skipped != null && !skipped.isEmpty()
                    ? "only local network addresses are known, and this phone is on cellular: join the hub's Wi-Fi"
                    : "no address the app can dial is known: pair again");
        }
        return out.toString();
    }

    /** The reject code for a lost race: PIN_MISMATCH if any route presented another key; TIMEOUT if all timed out. */
    public static String code(List<RouteRace.Failure> failures) {
        boolean allTimedOut = !failures.isEmpty();
        for (RouteRace.Failure failure : failures) {
            if (pinMismatch(failure.error)) return PIN_MISMATCH;
            allTimedOut &= timedOut(failure.error);
        }
        return allTimedOut ? TIMEOUT : UNREACHABLE;
    }

    private static String clip(String text) {
        if (text == null) return "";
        return text.length() > 160 ? text.substring(0, 160) : text;
    }
}
