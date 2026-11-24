package tools.twr.mobile.dev;

import java.security.GeneralSecurityException;
import java.util.Collections;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;
import okhttp3.ConnectionSpec;
import okhttp3.OkHttpClient;
import okhttp3.TlsVersion;
import tools.twr.mobile.dev.pairing.PinnedHostnameVerifier;
import tools.twr.mobile.dev.pairing.PinnedTrustManager;

/**
 * The OkHttp client for a pinned hub: its key and nothing else (no system trust, no fallback),
 * TLS 1.3 preferred, 1.2 only if the gateway offers nothing else. No Android types, so the JVM
 * unit tests run it against a real TLS server.
 */
public final class PinnedHttp {
    private PinnedHttp() {}

    /**
     * Derived from {@code base}: the dispatcher and connection pool are shared, but OkHttp keys
     * pooled connections by socket factory and hostname verifier, so a connection made under
     * one pin (or under system trust) is never reused under another.
     */
    public static OkHttpClient client(OkHttpClient base, String pin, int connectTimeoutSeconds) throws GeneralSecurityException {
        PinnedTrustManager trust = new PinnedTrustManager(pin);
        SSLContext context = SSLContext.getInstance("TLS");
        context.init(null, new TrustManager[] {trust}, null);
        ConnectionSpec spec = new ConnectionSpec.Builder(ConnectionSpec.MODERN_TLS)
                .tlsVersions(TlsVersion.TLS_1_3, TlsVersion.TLS_1_2)
                .build();
        return base.newBuilder()
                .sslSocketFactory(context.getSocketFactory(), trust)
                .hostnameVerifier(new PinnedHostnameVerifier(trust))
                .connectionSpecs(Collections.singletonList(spec))
                .connectTimeout(connectTimeoutSeconds, TimeUnit.SECONDS)
                .build();
    }
}
