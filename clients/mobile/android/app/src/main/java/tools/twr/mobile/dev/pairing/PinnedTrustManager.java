package tools.twr.mobile.dev.pairing;

import java.net.Socket;
import java.security.cert.CertificateException;
import java.security.cert.X509Certificate;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.X509ExtendedTrustManager;

/**
 * Trust for a hub paired by pin: a server chain is accepted if and only if its leaf's
 * SubjectPublicKeyInfo SHA-256 equals the saved pin. The chain, the name and the validity dates
 * are ignored (docs/design/phone-pairing.md, Client trust rule). Every other chain throws, so
 * the TLS handshake fails before the app writes a byte. Never falls back to system trust.
 */
public final class PinnedTrustManager extends X509ExtendedTrustManager {
    private final byte[] pin;
    private final String pinText;

    public PinnedTrustManager(String pin) {
        this.pin = Pins.decodePin(pin);
        this.pinText = pin;
    }

    public String pin() {
        return pinText;
    }

    /** Whether {@code leaf} carries the pinned key; any encoding problem is a no. */
    public boolean matches(java.security.cert.Certificate leaf) {
        try {
            return Pins.equal(Pins.pinBytes(leaf), pin);
        } catch (CertificateException unreadable) {
            return false;
        }
    }

    private void check(X509Certificate[] chain) throws CertificateException {
        if (chain == null || chain.length == 0 || chain[0] == null) {
            throw new PinMismatchException(null);
        }
        byte[] presented = Pins.pinBytes(chain[0]);
        if (!Pins.equal(presented, pin)) throw new PinMismatchException(Base64Url.encode(presented));
    }

    @Override
    public void checkServerTrusted(X509Certificate[] chain, String authType) throws CertificateException {
        check(chain);
    }

    @Override
    public void checkServerTrusted(X509Certificate[] chain, String authType, Socket socket) throws CertificateException {
        check(chain);
    }

    @Override
    public void checkServerTrusted(X509Certificate[] chain, String authType, SSLEngine engine) throws CertificateException {
        check(chain);
    }

    @Override
    public void checkClientTrusted(X509Certificate[] chain, String authType) throws CertificateException {
        throw new CertificateException("the app accepts no client certificates");
    }

    @Override
    public void checkClientTrusted(X509Certificate[] chain, String authType, Socket socket) throws CertificateException {
        throw new CertificateException("the app accepts no client certificates");
    }

    @Override
    public void checkClientTrusted(X509Certificate[] chain, String authType, SSLEngine engine) throws CertificateException {
        throw new CertificateException("the app accepts no client certificates");
    }

    @Override
    public X509Certificate[] getAcceptedIssuers() {
        return new X509Certificate[0];
    }
}
