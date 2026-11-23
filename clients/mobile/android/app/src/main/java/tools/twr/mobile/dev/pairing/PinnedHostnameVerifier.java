package tools.twr.mobile.dev.pairing;

import java.security.cert.Certificate;
import javax.net.ssl.HostnameVerifier;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLSession;

/**
 * A pinned hub's certificate names {@code twr-hub.invalid}, not the address dialed, so the name
 * check is the pin check again: true only when the session's peer leaf carries the pinned key.
 */
public final class PinnedHostnameVerifier implements HostnameVerifier {
    private final PinnedTrustManager trust;

    public PinnedHostnameVerifier(PinnedTrustManager trust) {
        this.trust = trust;
    }

    @Override
    public boolean verify(String hostname, SSLSession session) {
        if (session == null) return false;
        Certificate[] peers;
        try {
            peers = session.getPeerCertificates();
        } catch (SSLPeerUnverifiedException unverified) {
            return false;
        }
        return peers != null && peers.length > 0 && trust.matches(peers[0]);
    }
}
