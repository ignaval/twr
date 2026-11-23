package tools.twr.mobile.dev.pairing;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.KeyStore;
import java.security.cert.CertificateException;
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import javax.net.SocketFactory;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLException;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManager;
import org.junit.AfterClass;
import org.junit.BeforeClass;
import org.junit.Test;

/**
 * A real TLS handshake against a self-signed P-256 certificate like the gateway's (CN "twr hub",
 * SAN twr-hub.invalid), made by keytool or the openssl CLI: the pinned trust manager connects
 * with the right pin, whatever the name and dates, and with a wrong pin the handshake fails
 * before the client sends any application data.
 */
public class PinnedTlsTest {
    private static File dir;
    private static Server current;
    private static Server expired;

    @BeforeClass
    public static void certificates() throws Exception {
        dir = Files.createTempDirectory("twr-pinned-tls").toFile();
        current = new Server(certificate(dir, "current", false));
        expired = new Server(certificate(dir, "expired", true));
    }

    @AfterClass
    public static void cleanUp() throws Exception {
        if (current != null) current.close();
        if (expired != null) expired.close();
        if (dir != null) {
            File[] files = dir.listFiles();
            if (files != null) for (File file : files) file.delete();
            dir.delete();
        }
    }

    @Test
    public void spkiFromTheDerIsTheKeysEncoding() throws Exception {
        X509Certificate leaf = current.leaf;
        assertArrayEquals(leaf.getPublicKey().getEncoded(), Pins.spki(leaf.getEncoded()));
        assertEquals(Pins.pin(leaf.getPublicKey().getEncoded()), Pins.pin(leaf));
        assertEquals("EC", leaf.getPublicKey().getAlgorithm());
    }

    @Test
    public void theRightPinConnectsOverTls13() throws Exception {
        PinnedTrustManager trust = new PinnedTrustManager(current.pin);
        try (SSLSocket socket = client(trust, current.port)) {
            socket.startHandshake();
            assertEquals("TLSv1.3", socket.getSession().getProtocol());
            assertTrue(new PinnedHostnameVerifier(trust).verify("127.0.0.1", socket.getSession()));
            assertFalse("another pin's verifier", new PinnedHostnameVerifier(new PinnedTrustManager(expired.pin)).verify("127.0.0.1", socket.getSession()));
            OutputStream out = socket.getOutputStream();
            out.write("PING\n".getBytes(StandardCharsets.US_ASCII));
            out.flush();
            byte[] reply = new byte[5];
            InputStream in = socket.getInputStream();
            int read = 0;
            while (read < 5) {
                int n = in.read(reply, read, 5 - read);
                if (n < 0) break;
                read += n;
            }
            assertEquals("PONG\n", new String(reply, 0, read, StandardCharsets.US_ASCII));
        }
        assertTrue(current.awaitAppBytes(5));
    }

    @Test
    public void validityDatesAreIgnored() throws Exception {
        assertTrue(expired.leaf.getNotAfter().getTime() < System.currentTimeMillis());
        try (SSLSocket socket = client(new PinnedTrustManager(expired.pin), expired.port)) {
            socket.startHandshake();
            assertNotNull(socket.getSession().getPeerCertificates());
        }
    }

    @Test
    public void aWrongPinFailsTheHandshakeBeforeAnyApplicationData() throws Exception {
        int localPort = -1;
        try (SSLSocket socket = client(new PinnedTrustManager(expired.pin), current.port)) {
            localPort = socket.getLocalPort();
            // No explicit handshake: the first write starts it, as an HTTP client's request would.
            socket.getOutputStream().write("GET /v1/pair HTTP/1.1\r\n\r\n".getBytes(StandardCharsets.US_ASCII));
            socket.getOutputStream().flush();
            fail("the write went through with a wrong pin");
        } catch (SSLException refused) {
            PinMismatchException mismatch = cause(refused, PinMismatchException.class);
            assertNotNull("PinMismatchException in " + refused, mismatch);
            assertEquals(current.pin, mismatch.presented);
            assertTrue(Failures.pinMismatch(refused));
            assertTrue(Failures.reason(refused, "box").startsWith("PIN_MISMATCH: this is another device, not box"));
        }
        assertEquals("application bytes the server received", Integer.valueOf(0), current.received(localPort));
    }

    @Test
    public void captureNotesTheKeyAndSendsNothing() throws Exception {
        java.util.Set<Integer> before = current.settled();
        assertEquals(current.pin, PinCapture.capture(SocketFactory.getDefault(), "127.0.0.1", current.port, 5000));
        java.util.Set<Integer> after = current.settled();
        after.removeAll(before);
        assertEquals("one new connection", 1, after.size());
        assertEquals("application bytes the server received", Integer.valueOf(0), current.received(after.iterator().next()));
    }

    @Test
    public void captureReportsAnUnreachableAddress() throws Exception {
        int port;
        try (java.net.ServerSocket closed = new java.net.ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))) {
            port = closed.getLocalPort();
        }
        try {
            PinCapture.capture(SocketFactory.getDefault(), "127.0.0.1", port, 2000);
            fail("captured a pin from a closed port");
        } catch (IOException expected) {
            assertEquals("unreachable", Failures.reason(expected, "box"));
        }
    }

    @Test
    public void theTrustManagerRefusesEverythingElse() throws Exception {
        PinnedTrustManager trust = new PinnedTrustManager(current.pin);
        assertEquals(0, trust.getAcceptedIssuers().length);
        assertTrue(trust.matches(current.leaf));
        assertFalse(trust.matches(expired.leaf));
        trust.checkServerTrusted(new X509Certificate[] {current.leaf}, "ECDHE_ECDSA");
        trust.checkServerTrusted(new X509Certificate[] {current.leaf, expired.leaf}, "ECDHE_ECDSA");
        for (X509Certificate[] chain : Arrays.asList(null, new X509Certificate[0], new X509Certificate[] {expired.leaf},
                new X509Certificate[] {expired.leaf, current.leaf})) {
            try {
                trust.checkServerTrusted(chain, "ECDHE_ECDSA");
                fail("accepted " + (chain == null ? "null" : chain.length + " certificates"));
            } catch (PinMismatchException expected) {
                assertTrue(expected.getMessage().startsWith("PIN_MISMATCH"));
            }
        }
        try {
            trust.checkClientTrusted(new X509Certificate[] {current.leaf}, "ECDHE_ECDSA");
            fail("accepted a client certificate");
        } catch (CertificateException expected) {
            // refused
        }
        try {
            new PinnedTrustManager("not a pin");
            fail("accepted a malformed pin");
        } catch (IllegalArgumentException expected) {
            // refused
        }
    }

    private static SSLSocket client(TrustManager trust, int port) throws Exception {
        SSLContext context = SSLContext.getInstance("TLS");
        context.init(null, new TrustManager[] {trust}, null);
        SSLSocket socket = (SSLSocket) context.getSocketFactory().createSocket(InetAddress.getByName("127.0.0.1"), port);
        socket.setSoTimeout(5000);
        return socket;
    }

    private static <T extends Throwable> T cause(Throwable error, Class<T> type) {
        for (Throwable cause = error; cause != null; cause = cause.getCause()) {
            if (type.isInstance(cause)) return type.cast(cause);
        }
        return null;
    }

    /** A PKCS#12 keystore holding a self-signed P-256 certificate like the gateway's identity. */
    static File certificate(File dir, String name, boolean expired) throws Exception {
        File store = new File(dir, name + ".p12");
        String keytool = new File(System.getProperty("java.home"), "bin/keytool").getPath();
        List<String> command = new ArrayList<>(Arrays.asList(keytool, "-genkeypair", "-alias", "hub", "-keyalg", "EC",
                "-groupname", "secp256r1", "-sigalg", "SHA256withECDSA", "-dname", "CN=twr hub",
                "-ext", "SAN=dns:twr-hub.invalid", "-ext", "EKU=serverAuth", "-keystore", store.getPath(),
                "-storetype", "PKCS12", "-storepass", "changeit", "-keypass", "changeit"));
        if (expired) command.addAll(Arrays.asList("-startdate", "-10d", "-validity", "1"));
        else command.addAll(Arrays.asList("-validity", "800"));
        if (new File(keytool).canExecute() && run(dir, command)) return store;
        // No keytool beside this JVM: the openssl CLI.
        File key = new File(dir, name + "-key.pem");
        File cert = new File(dir, name + "-cert.pem");
        List<String> request = new ArrayList<>(Arrays.asList("openssl", "req", "-x509", "-newkey", "ec",
                "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", key.getPath(), "-out", cert.getPath(),
                "-subj", "/CN=twr hub", "-addext", "subjectAltName=DNS:twr-hub.invalid",
                "-addext", "extendedKeyUsage=serverAuth", "-sha256"));
        if (expired) request.addAll(Arrays.asList("-not_before", "20200101000000Z", "-not_after", "20200102000000Z"));
        else request.addAll(Arrays.asList("-days", "800"));
        if (!run(dir, request)) throw new IOException("neither keytool nor openssl could make a certificate");
        if (!run(dir, Arrays.asList("openssl", "pkcs12", "-export", "-in", cert.getPath(), "-inkey", key.getPath(),
                "-name", "hub", "-out", store.getPath(), "-passout", "pass:changeit"))) {
            throw new IOException("openssl pkcs12 failed");
        }
        return store;
    }

    private static boolean run(File dir, List<String> command) throws Exception {
        Process process;
        try {
            process = new ProcessBuilder(command).redirectErrorStream(true).redirectOutput(new File(dir, "tool.log")).start();
        } catch (IOException missing) {
            return false;
        }
        return process.waitFor(60, TimeUnit.SECONDS) && process.exitValue() == 0;
    }

    /**
     * A TLS 1.3 server on 127.0.0.1 that counts the application bytes it receives and answers
     * PING with PONG.
     */
    private static final class Server implements AutoCloseable {
        final SSLServerSocket socket;
        final int port;
        final X509Certificate leaf;
        final String pin;
        final AtomicInteger appBytes = new AtomicInteger();
        /** Client ports accepted, and the application bytes each carried once it ended. */
        private final java.util.Set<Integer> accepted = new java.util.HashSet<>();
        private final java.util.Map<Integer, Integer> ended = new java.util.HashMap<>();
        private final Thread thread;

        Server(File store) throws Exception {
            KeyStore keys = KeyStore.getInstance("PKCS12");
            try (InputStream in = new FileInputStream(store)) {
                keys.load(in, "changeit".toCharArray());
            }
            leaf = (X509Certificate) keys.getCertificate(keys.aliases().nextElement());
            pin = Pins.pin(leaf);
            KeyManagerFactory factory = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
            factory.init(keys, "changeit".toCharArray());
            SSLContext context = SSLContext.getInstance("TLS");
            context.init(factory.getKeyManagers(), null, null);
            socket = (SSLServerSocket) context.getServerSocketFactory().createServerSocket(0, 16, InetAddress.getByName("127.0.0.1"));
            socket.setEnabledProtocols(new String[] {"TLSv1.3"});
            port = socket.getLocalPort();
            thread = new Thread(this::serve, "pinned-tls-server");
            thread.setDaemon(true);
            thread.start();
        }

        private void serve() {
            while (!socket.isClosed()) {
                Socket connection;
                try {
                    connection = socket.accept();
                } catch (IOException closed) {
                    return;
                }
                synchronized (this) {
                    accepted.add(connection.getPort());
                }
                Thread handler = new Thread(() -> handle(connection));
                handler.setDaemon(true);
                handler.start();
            }
        }

        private void handle(Socket accepted) {
            int received = 0;
            try (Socket connection = accepted) {
                connection.setSoTimeout(5000);
                InputStream in = connection.getInputStream();
                byte[] buffer = new byte[256];
                StringBuilder line = new StringBuilder();
                for (int n; (n = in.read(buffer)) > 0; ) {
                    received += n;
                    appBytes.addAndGet(n);
                    line.append(new String(buffer, 0, n, StandardCharsets.US_ASCII));
                    if (line.toString().equals("PING\n")) {
                        connection.getOutputStream().write("PONG\n".getBytes(StandardCharsets.US_ASCII));
                        connection.getOutputStream().flush();
                    }
                }
            } catch (IOException ended) {
                // The client refused the handshake or went away.
            } finally {
                synchronized (this) {
                    ended.put(accepted.getPort(), received);
                    notifyAll();
                }
            }
        }

        /** The application bytes the connection from {@code clientPort} carried, once it has ended. */
        synchronized Integer received(int clientPort) throws InterruptedException {
            long end = System.currentTimeMillis() + 10_000;
            while (!ended.containsKey(clientPort) && System.currentTimeMillis() < end) wait(100);
            return ended.get(clientPort);
        }

        /** Waits until every accepted connection has ended; their client ports. */
        synchronized java.util.Set<Integer> settled() throws InterruptedException {
            long end = System.currentTimeMillis() + 10_000;
            while (!ended.keySet().containsAll(accepted) && System.currentTimeMillis() < end) wait(100);
            return new java.util.HashSet<>(ended.keySet());
        }

        boolean awaitAppBytes(int count) throws InterruptedException {
            long end = System.currentTimeMillis() + 10_000;
            while (appBytes.get() < count && System.currentTimeMillis() < end) Thread.sleep(20);
            return appBytes.get() >= count;
        }

        @Override
        public void close() throws IOException {
            socket.close();
        }
    }
}
