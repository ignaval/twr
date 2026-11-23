package tools.twr.mobile.dev.pairing;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import java.net.ConnectException;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import org.junit.After;
import org.junit.Test;

/** The racing rule with scripted probes: head start, parallel rest, first success wins, losers cancelled. */
public class RouteRaceTest {
    private final ExecutorService executor = Executors.newCachedThreadPool();

    @After
    public void stop() {
        executor.shutdownNow();
    }

    private static SavedRoutes.Plan plan(String remembered, String... routes) {
        SavedRoutes saved = new SavedRoutes();
        saved.merge(Arrays.asList(routes), 1000);
        if (remembered != null) saved.connected(Route.parse(remembered), NetworkType.WIFI, 1000);
        return saved.plan(NetworkType.WIFI);
    }

    /** A probe whose behaviour per route is a delay and then success or a failure. */
    private static final class Script implements RouteRace.Probe<String> {
        final Map<String, Long> delays = new ConcurrentHashMap<>();
        final Map<String, Exception> failures = new ConcurrentHashMap<>();
        final Map<String, Long> startedAt = new ConcurrentHashMap<>();
        final Map<String, Boolean> cancelled = new ConcurrentHashMap<>();
        final long origin = System.nanoTime();

        @Override
        public String attempt(Route route, RouteRace.Attempt attempt) throws Exception {
            String key = route.toString();
            startedAt.put(key, (System.nanoTime() - origin) / 1_000_000L);
            CountDownLatch cancel = new CountDownLatch(1);
            attempt.onCancel(() -> {
                cancelled.put(key, true);
                cancel.countDown();
            });
            long delay = delays.getOrDefault(key, 0L);
            if (cancel.await(delay, TimeUnit.MILLISECONDS)) throw new java.io.IOException("Canceled");
            Exception failure = failures.get(key);
            if (failure != null) throw failure;
            return "ok " + key;
        }
    }

    @Test
    public void theRememberedRouteGetsAHeadStartAndWins() throws Exception {
        Script script = new Script();
        script.delays.put("tls:10.0.0.1:1", 50L);
        script.delays.put("tls:10.0.0.2:1", 10L);
        RouteRace.Won<String> won = new RouteRace<>(executor, script).run(plan("tls:10.0.0.1:1", "tls:10.0.0.2:1", "tls:10.0.0.1:1"), 250, 5000);
        assertEquals("tls:10.0.0.1:1", won.route.toString());
        assertEquals("ok tls:10.0.0.1:1", won.value);
        assertFalse("the rest never started", script.startedAt.containsKey("tls:10.0.0.2:1"));
    }

    @Test
    public void theRestStartAfterTheHeadStartAndTheFastestWins() throws Exception {
        Script script = new Script();
        script.delays.put("tls:10.0.0.1:1", 3000L);
        script.delays.put("tls:10.0.0.2:1", 400L);
        script.delays.put("tls:10.0.0.3:1", 20L);
        RouteRace.Won<String> won = new RouteRace<>(executor, script).run(plan("tls:10.0.0.1:1", "tls:10.0.0.1:1", "tls:10.0.0.2:1", "tls:10.0.0.3:1"), 250, 5000);
        assertEquals("tls:10.0.0.3:1", won.route.toString());
        long rest = script.startedAt.get("tls:10.0.0.3:1");
        assertTrue("started at " + rest + " ms", rest >= 240);
        Thread.sleep(50);
        assertEquals(Boolean.TRUE, script.cancelled.get("tls:10.0.0.1:1"));
        assertEquals(Boolean.TRUE, script.cancelled.get("tls:10.0.0.2:1"));
        assertFalse(script.cancelled.containsKey("tls:10.0.0.3:1"));
    }

    @Test
    public void aFailedFirstRouteStartsTheRestAtOnce() throws Exception {
        Script script = new Script();
        script.failures.put("tls:10.0.0.1:1", new ConnectException("refused"));
        RouteRace.Won<String> won = new RouteRace<>(executor, script).run(plan("tls:10.0.0.1:1", "tls:10.0.0.1:1", "tls:10.0.0.2:1"), 250, 5000);
        assertEquals("tls:10.0.0.2:1", won.route.toString());
        assertTrue(script.startedAt.get("tls:10.0.0.2:1") < 200);
    }

    @Test
    public void withNothingRememberedEveryRouteStartsAtOnce() throws Exception {
        Script script = new Script();
        script.delays.put("tls:10.0.0.1:1", 100L);
        script.delays.put("tls:10.0.0.2:1", 30L);
        RouteRace.Won<String> won = new RouteRace<>(executor, script).run(plan(null, "tls:10.0.0.1:1", "tls:10.0.0.2:1"), 250, 5000);
        assertEquals("tls:10.0.0.2:1", won.route.toString());
        assertTrue(script.startedAt.get("tls:10.0.0.1:1") < 200);
    }

    @Test
    public void whenEveryRouteFailsEachIsNamedWithItsReason() throws Exception {
        Script script = new Script();
        script.failures.put("tls:10.0.0.1:1", new ConnectException("refused"));
        script.failures.put("tls:10.0.0.2:1", new javax.net.ssl.SSLHandshakeException("bad"));
        script.failures.get("tls:10.0.0.2:1").initCause(new PinMismatchException("abc"));
        script.delays.put("tls:10.0.0.3:1", 5000L);
        try {
            new RouteRace<>(executor, script).run(plan(null, "tls:10.0.0.1:1", "tls:10.0.0.2:1", "tls:10.0.0.3:1"), 250, 600);
            fail("won");
        } catch (RouteRace.Failed failed) {
            assertFalse(failed.cancelled);
            assertEquals(3, failed.failures.size());
            String summary = Failures.summary(failed.failures, failed.skipped, "box");
            assertEquals("tls:10.0.0.1:1 unreachable; tls:10.0.0.2:1 PIN_MISMATCH: this is another device, not box; tls:10.0.0.3:1 timed out", summary);
            assertEquals(Failures.PIN_MISMATCH, Failures.code(failed.failures));
        }
        Thread.sleep(50);
        assertEquals("the pending route was cancelled at the deadline", Boolean.TRUE, script.cancelled.get("tls:10.0.0.3:1"));
    }

    @Test
    public void allTimedOutIsTimeout() throws Exception {
        Script script = new Script();
        script.delays.put("tls:10.0.0.1:1", 5000L);
        try {
            new RouteRace<>(executor, script).run(plan(null, "tls:10.0.0.1:1"), 250, 300);
            fail("won");
        } catch (RouteRace.Failed failed) {
            assertEquals(Failures.TIMEOUT, Failures.code(failed.failures));
        }
    }

    @Test
    public void anotherHubAnsweringLosesThatRoute() throws Exception {
        Script script = new Script();
        script.failures.put("tls:10.0.0.1:1", new HubMismatchException("another hub answered"));
        try {
            new RouteRace<>(executor, script).run(plan(null, "tls:10.0.0.1:1"), 250, 2000);
            fail("won");
        } catch (RouteRace.Failed failed) {
            assertEquals("tls:10.0.0.1:1 another hub answered", Failures.summary(failed.failures, failed.skipped, "box"));
            assertEquals(Failures.UNREACHABLE, Failures.code(failed.failures));
        }
    }

    @Test
    public void cancelEndsTheRaceAndEveryAttempt() throws Exception {
        Script script = new Script();
        script.delays.put("tls:10.0.0.1:1", 5000L);
        script.delays.put("tls:10.0.0.2:1", 5000L);
        RouteRace<String> race = new RouteRace<>(executor, script);
        executor.execute(() -> {
            try {
                Thread.sleep(400);
            } catch (InterruptedException ignored) {
                return;
            }
            race.cancel();
        });
        long start = System.nanoTime();
        try {
            race.run(plan("tls:10.0.0.1:1", "tls:10.0.0.1:1", "tls:10.0.0.2:1"), 250, 10_000);
            fail("won");
        } catch (RouteRace.Failed failed) {
            assertTrue(failed.cancelled);
        }
        assertTrue((System.nanoTime() - start) / 1_000_000L < 3000);
        Thread.sleep(50);
        assertEquals(Boolean.TRUE, script.cancelled.get("tls:10.0.0.1:1"));
        assertEquals(Boolean.TRUE, script.cancelled.get("tls:10.0.0.2:1"));
    }

    @Test
    public void anEmptyPlanFailsAtOnce() throws Exception {
        try {
            new RouteRace<>(executor, new Script()).run(plan(null, "relay:x"), 250, 5000);
            fail("won");
        } catch (RouteRace.Failed failed) {
            assertEquals(Collections.emptyList(), failed.failures);
            List<Route> none = Collections.emptyList();
            assertTrue(Failures.summary(failed.failures, none, "box").contains("pair again"));
        }
    }
}
