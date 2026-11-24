package tools.twr.mobile.dev.pairing;

import java.io.IOException;

/** A route passed the pin check but its {@code /health} isn't the expected hub's (another gateway id, or not a twr gateway). */
public final class HubMismatchException extends IOException {
    public HubMismatchException(String message) {
        super(message);
    }
}
