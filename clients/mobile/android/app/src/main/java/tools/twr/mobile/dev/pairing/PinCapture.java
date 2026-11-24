package tools.twr.mobile.dev.pairing;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.security.GeneralSecurityException;
import java.security.cert.CertificateException;
import java.security.cert.X509Certificate;
import javax.net.SocketFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLException;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509ExtendedTrustManager;

/**
 * Step 1 of typed pairing: learn the key an address presents without trusting it. The handshake
 * is always refused, after the leaf's pin is noted, so nothing is ever sent on this connection.
 * The caller then reconnects with {@link PinnedTrustManager} for that pin, checks the hub's
 * proof, and only then sends the code.
 */
public final class PinCapture {
    private PinCapture() {}

    /** Thrown by the capturing trust manager on every chain: the key is noted, never trusted. */
    static final class Captured extends CertificateException {
        Captured() {
            super("key noted for the pairing proof; not trusted");
        }
    }

    /** Records the leaf's pin, then refuses the chain. */
    static final class Recorder extends X509ExtendedTrustManager {
        volatile String pin;

        private void record(X509Certificate[] chain) throws CertificateException {
            if (chain == null || chain.length == 0 || chain[0] == null) throw new PinMismatchException(null);
            pin = Pins.pin(chain[0]);
            throw new Captured();
        }

        @Override public void checkServerTrusted(X509Certificate[] chain, String authType) throws CertificateException { record(chain); }
        @Override public void checkServerTrusted(X509Certificate[] chain, String authType, Socket socket) throws CertificateException { record(chain); }
        @Override public void checkServerTrusted(X509Certificate[] chain, String authType, SSLEngine engine) throws CertificateException { record(chain); }
        @Override public void checkClientTrusted(X509Certificate[] chain, String authType) throws CertificateException { throw new CertificateException("no client certificates"); }
        @Override public void checkClientTrusted(X509Certificate[] chain, String authType, Socket socket) throws CertificateException { throw new CertificateException("no client certificates"); }
        @Override public void checkClientTrusted(X509Certificate[] chain, String authType, SSLEngine engine) throws CertificateException { throw new CertificateException("no client certificates"); }
        @Override public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }
    }

    /** The pin {@code host:port} presents. Throws if it can't be reached or presents no certificate. */
    public static String capture(SocketFactory sockets, String host, int port, int timeoutMillis) throws IOException {
        Recorder recorder = new Recorder();
        SSLContext context;
        try {
            context = SSLContext.getInstance("TLS");
            context.init(null, new TrustManager[] {recorder}, null);
        } catch (GeneralSecurityException unavailable) {
            throw new IOException("TLS is unavailable", unavailable);
        }
        Socket raw = sockets.createSocket();
        try {
            raw.connect(new InetSocketAddress(host, port), timeoutMillis);
            raw.setSoTimeout(timeoutMillis);
            SSLSocket tls = (SSLSocket) context.getSocketFactory().createSocket(raw, host, port, true);
            try {
                tls.startHandshake();
            } catch (SSLException refused) {
                if (recorder.pin != null) return recorder.pin;
                throw refused;
            } finally {
                tls.close();
            }
            // The recorder throws on every chain, so a completed handshake means no check ran.
            throw new SSLException("the TLS handshake completed without a certificate check");
        } finally {
            raw.close();
        }
    }
}
