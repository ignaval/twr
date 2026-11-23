package tools.twr.mobile.dev.pairing;

import java.security.cert.CertificateException;

/** The server presented a key other than the pinned one. Its message starts with {@code PIN_MISMATCH}. */
public final class PinMismatchException extends CertificateException {
    /** The presented key's pin, or null when there was no certificate. */
    public final String presented;

    public PinMismatchException(String presented) {
        super(presented == null ? "PIN_MISMATCH: the server presented no certificate"
                : "PIN_MISMATCH: the server presented key " + presented);
        this.presented = presented;
    }
}
