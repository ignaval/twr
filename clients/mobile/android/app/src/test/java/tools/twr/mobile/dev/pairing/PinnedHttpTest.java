package tools.twr.mobile.dev.pairing;

import static org.junit.Assert.assertEquals;
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
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import javax.net.SocketFactory;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.TlsVersion;
import org.junit.AfterClass;
import org.junit.BeforeClass;
import org.junit.Test;
import tools.twr.mobile.dev.PinnedHttp;

/**
 * The app's pinned OkHttp client against a real HTTPS server with a gateway-like self-signed
 * certificate: it reaches the hub by IP or name with the right pin, and with a wrong pin OkHttp
 * sends nothing, not the request line nor the token. Typed pairing's transport steps (note the
 * key, then pin it) work end to end.
 */
public class PinnedHttpTest {
    private static File dir;
    private static Gateway gateway;
    private static String otherPin;
    private static final OkHttpClient BASE = new OkHttpClient.Builder().followRedirects(false).followSslRedirects(false)
            .connectTimeout(5, TimeUnit.SECONDS).readTimeout(5, TimeUnit.SECONDS).retryOnConnectionFailure(true).build();

    @BeforeClass
    public static void start() throws Exception {
        dir = Files.createTempDirectory("twr-pinned-http").toFile();
        gateway = new Gateway(PinnedTlsTest.certificate(dir, "gateway", false));
        KeyStore other = Gateway.load(PinnedTlsTest.certificate(dir, "other", false));
        otherPin = Pins.pin(other.getCertificate(other.aliases().nextElement()));
    }

    @AfterClass
    public static void stop() throws Exception {
        if (gateway != null) gateway.close();
        if (dir != null) {
            File[] files = dir.listFiles();
            if (files != null) for (File file : files) file.delete();
            dir.delete();
        }
    }

    private static Request health(String host) {
        return new Request.Builder().url("https://" + host + ":" + gateway.port + "/health").build();
    }

    @Test
    public void theRightPinReachesTheHubByAddressOrNameOverTls13() throws Exception {
        OkHttpClient client = PinnedHttp.client(BASE, gateway.pin, 5);
        for (String host : new String[] {"127.0.0.1", "localhost"}) {
            try (Response response = client.newCall(health(host)).execute()) {
                assertEquals(host, 200, response.code());
                assertNotNull(response.handshake());
                assertEquals(TlsVersion.TLS_1_3, response.handshake().tlsVersion());
                assertTrue(response.body().string().contains("\"gateway_id\":\"" + Gateway.ID + "\""));
            }
        }
    }

    @Test
    public void aWrongPinSendsNothing() throws Exception {
        OkHttpClient client = PinnedHttp.client(BASE, otherPin, 5);
        java.util.Set<Integer> before = gateway.settled();
        Request request = new Request.Builder().url("https://127.0.0.1:" + gateway.port + "/v1/pair")
                .header("Authorization", "Bearer " + "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .post(okhttp3.RequestBody.create("{\"code\":\"ABCDEFGHJK23\",\"name\":\"phone\"}", okhttp3.MediaType.get("application/json")))
                .build();
        try (Response response = client.newCall(request).execute()) {
            fail("a wrong pin got HTTP " + response.code());
        } catch (IOException refused) {
            assertTrue(refused.toString(), Failures.pinMismatch(refused));
            assertTrue(Failures.reason(refused, "box").startsWith("PIN_MISMATCH"));
        }
        java.util.Set<Integer> after = gateway.settled();
        Map<Integer, Integer> received = gateway.received();
        after.removeAll(before);
        assertTrue("OkHttp connected", !after.isEmpty());
        for (Integer port : after) assertEquals("bytes the gateway received", Integer.valueOf(0), received.get(port));
    }

    @Test
    public void systemTrustRefusesTheSelfSignedHub() throws Exception {
        try (Response response = BASE.newCall(health("127.0.0.1")).execute()) {
            fail("system trust accepted a self-signed hub: HTTP " + response.code());
        } catch (javax.net.ssl.SSLException refused) {
            // The pinned client is the only way in.
        }
    }

    @Test
    public void typedPairingNotesTheKeyThenPinsIt() throws Exception {
        java.util.Set<Integer> before = gateway.settled();
        String pin = PinCapture.capture(SocketFactory.getDefault(), "127.0.0.1", gateway.port, 5000);
        assertEquals(gateway.pin, pin);
        java.util.Set<Integer> captured = gateway.settled();
        captured.removeAll(before);
        assertEquals(1, captured.size());
        assertEquals(Integer.valueOf(0), gateway.received().get(captured.iterator().next()));
        try (Response response = PinnedHttp.client(BASE, pin, 5).newCall(health("127.0.0.1")).execute()) {
            assertEquals(200, response.code());
        }
    }

    /** A minimal HTTPS server: answers every request with the gateway's /health JSON and records the bytes per connection. */
    private static final class Gateway implements AutoCloseable {
        static final String ID = "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90";
        final SSLServerSocket socket;
        final int port;
        final String pin;
        private final java.util.Set<Integer> accepted = new java.util.HashSet<>();
        private final Map<Integer, Integer> ended = new HashMap<>();

        static KeyStore load(File store) throws Exception {
            KeyStore keys = KeyStore.getInstance("PKCS12");
            try (InputStream in = new FileInputStream(store)) {
                keys.load(in, "changeit".toCharArray());
            }
            return keys;
        }

        Gateway(File store) throws Exception {
            KeyStore keys = load(store);
            pin = Pins.pin((X509Certificate) keys.getCertificate(keys.aliases().nextElement()));
            KeyManagerFactory factory = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
            factory.init(keys, "changeit".toCharArray());
            SSLContext context = SSLContext.getInstance("TLS");
            context.init(factory.getKeyManagers(), null, null);
            socket = (SSLServerSocket) context.getServerSocketFactory().createServerSocket(0, 16, InetAddress.getByName("127.0.0.1"));
            port = socket.getLocalPort();
            Thread thread = new Thread(this::serve, "pinned-http-gateway");
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

        private void handle(Socket connection) {
            int received = 0;
            try (Socket open = connection) {
                open.setSoTimeout(5000);
                InputStream in = open.getInputStream();
                StringBuilder request = new StringBuilder();
                byte[] buffer = new byte[1024];
                while (request.indexOf("\r\n\r\n") < 0) {
                    int n = in.read(buffer);
                    if (n < 0) break;
                    received += n;
                    request.append(new String(buffer, 0, n, StandardCharsets.ISO_8859_1));
                }
                if (request.indexOf("\r\n\r\n") >= 0) {
                    byte[] body = ("{\"service\":\"twr-mobile\",\"protocol\":2,\"gateway_id\":\"" + ID + "\",\"pin\":\"" + pin + "\"}")
                            .getBytes(StandardCharsets.UTF_8);
                    OutputStream out = open.getOutputStream();
                    out.write(("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: " + body.length
                            + "\r\nConnection: close\r\n\r\n").getBytes(StandardCharsets.US_ASCII));
                    out.write(body);
                    out.flush();
                }
            } catch (IOException ended) {
                // A refused handshake, or the client went away.
            } finally {
                synchronized (this) {
                    ended.put(connection.getPort(), received);
                    notifyAll();
                }
            }
        }

        /** Waits until every accepted connection has ended; their client ports. */
        synchronized java.util.Set<Integer> settled() throws InterruptedException {
            long end = System.currentTimeMillis() + 10_000;
            while (!ended.keySet().containsAll(accepted) && System.currentTimeMillis() < end) wait(100);
            return new java.util.HashSet<>(ended.keySet());
        }

        synchronized Map<Integer, Integer> received() {
            return new HashMap<>(ended);
        }

        @Override
        public void close() throws IOException {
            socket.close();
        }
    }
}
