package tools.twr.mobile.dev.pairing;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import org.junit.Test;

/** Route grammar beyond the fixture, typed addresses, the cellular filter, merging and the dial plan. */
public class RoutesTest {
    private static final long DAY = 24L * 60 * 60 * 1000;

    @Test
    public void typedAddressesDefaultThePort() {
        assertEquals("tls:192.168.1.20:47321", Route.fromAddress("192.168.1.20").toString());
        assertEquals("tls:192.168.1.20:443", Route.fromAddress(" 192.168.1.20:443 ").toString());
        assertEquals("tls:box.tail1234.ts.net:47321", Route.fromAddress("Box.Tail1234.ts.net").toString());
        assertEquals("tls:[2001:db8::5]:47321", Route.fromAddress("2001:db8::5").toString());
        assertEquals("tls:[2001:db8::5]:47321", Route.fromAddress("[2001:db8::5]").toString());
        assertEquals("tls:[2001:db8::5]:8443", Route.fromAddress("[2001:DB8::5]:8443").toString());
        assertEquals("https://[2001:db8::5]:8443", Route.fromAddress("[2001:db8::5]:8443").origin());
        assertEquals("https://box.local:47321", Route.fromAddress("box.local").origin());
        for (String bad : new String[] {"", " ", "box:0", "box:65536", "box:+80", "[::1", "[::1]x", "https://box", "box/x", "1.2.3", "01.2.3.4"}) {
            try {
                Route.fromAddress(bad);
                fail("accepted " + bad);
            } catch (IllegalArgumentException expected) {
                // refused
            }
        }
    }

    @Test
    public void ipLiteralsAreStrict() {
        for (String ok : new String[] {"::", "::1", "1::", "2001:db8::5", "1:2:3:4:5:6:7:8", "::ffff:1.2.3.4", "fe80::1:2", "1:2:3:4:5:6:1.2.3.4"}) {
            assertTrue(ok, Route.ipv6(ok) != null);
        }
        for (String bad : new String[] {":::", "1:::2", "1::2::3", "1:2:3:4:5:6:7:8:9", ":1", "1:", "12345::", "g::", "::1.2.3", "1:2:3:4:5:6:7:1.2.3.4", "::1%eth0"}) {
            assertNull(bad, Route.ipv6(bad));
        }
        assertTrue(Route.ipv4("0.0.0.0") != null);
        assertNull(Route.ipv4("256.1.1.1"));
        assertNull(Route.ipv4("1.2.3.4.5"));
        assertNull(Route.ipv4("1.2.3.04"));
    }

    @Test
    public void privateLanMeansRfc1918UniqueLocalOutsideTheTailnetAndDotLocal() {
        String[] lan = {"10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.20", "[fd00::1]", "[fc12::1]", "[::ffff:192.168.0.1]", "box.local", "BOX.LOCAL"};
        String[] notLan = {"100.64.0.1", "172.32.0.1", "172.15.0.1", "8.8.8.8", "[fd7a:115c:a1e0::5]", "[2001:db8::5]", "[fe80::1]", "box.tail1234.ts.net", "local.example.com"};
        for (String host : lan) assertTrue(host, Route.fromAddress(host).privateLan());
        for (String host : notLan) assertFalse(host, Route.fromAddress(host).privateLan());
        assertFalse(Route.parse("relay:anything").privateLan());
    }

    @Test
    public void mergeDeduplicatesKeepsUnknownKindsAndSkipsInvalid() {
        SavedRoutes saved = new SavedRoutes();
        List<String> rejected = saved.merge(Arrays.asList("tls:192.168.1.20:47321", "tls:Box.Tail1234.ts.net:47321", "relay:later", "tls:nope", "tls:box.tail1234.ts.net:47321"), 1000);
        assertEquals(Collections.singletonList("tls:nope"), rejected);
        assertEquals(Arrays.asList("tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321", "relay:later"), saved.routes());
        saved.merge(Arrays.asList("tls:192.168.1.20:47321", "tls:[2001:db8::5]:47321"), 2000);
        assertEquals(4, saved.routes().size());
        assertEquals(2000, saved.entries().get(0).seen);
        assertEquals(1000, saved.entries().get(1).seen);
    }

    @Test
    public void atMostSixteenTheLeastRecentlySeenGo() {
        SavedRoutes saved = new SavedRoutes();
        List<String> first = new ArrayList<>();
        for (int i = 1; i <= 10; i++) first.add("tls:10.0.0." + i + ":47321");
        saved.merge(first, 1000);
        List<String> second = new ArrayList<>();
        for (int i = 1; i <= 10; i++) second.add("tls:10.0.1." + i + ":47321");
        saved.merge(second, 2000);
        assertEquals(16, saved.routes().size());
        // Four of the older batch went, the last listed first among equals.
        assertEquals("tls:10.0.0.1:47321", saved.routes().get(0));
        assertEquals("tls:10.0.0.6:47321", saved.routes().get(5));
        assertEquals("tls:10.0.1.1:47321", saved.routes().get(6));
        assertFalse(saved.routes().contains("tls:10.0.0.7:47321"));
        assertTrue(saved.routes().contains("tls:10.0.1.10:47321"));
    }

    @Test
    public void routesUnseenForThirtyDaysGoButAnIdlePhoneKeepsThem() {
        SavedRoutes saved = new SavedRoutes();
        saved.merge(Arrays.asList("tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321"), 0);
        saved.connected(Route.parse("tls:192.168.1.20:47321"), NetworkType.WIFI, 0);
        // Nothing heard for 60 days: everything is as stale as everything else, so nothing goes.
        saved.prune();
        assertEquals(2, saved.routes().size());
        // The hub reports only the LAN route 31 days later: the other one goes, and so does its "last".
        saved.connected(Route.parse("tls:box.tail1234.ts.net:47321"), NetworkType.CELLULAR, 0);
        saved.merge(Collections.singletonList("tls:192.168.1.20:47321"), 31 * DAY);
        assertEquals(Collections.singletonList("tls:192.168.1.20:47321"), saved.routes());
        assertNull(saved.lastByNetwork().get("cellular"));
        assertEquals("tls:192.168.1.20:47321", saved.lastByNetwork().get("wifi"));
        assertNull(saved.lastRoute());
    }

    @Test
    public void aConnectionAfterAMonthAwayKeepsTheOtherRoutes() {
        SavedRoutes saved = new SavedRoutes();
        saved.merge(Arrays.asList("tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321"), 0);
        saved.connected(Route.parse("tls:box.tail1234.ts.net:47321"), NetworkType.CELLULAR, 40 * DAY);
        assertEquals(Arrays.asList("tls:192.168.1.20:47321", "tls:box.tail1234.ts.net:47321"), saved.routes());
        assertEquals("tls:box.tail1234.ts.net:47321", saved.lastByNetwork().get("cellular"));
        // The hello then reports what the gateway has now; the LAN route it no longer has goes.
        saved.merge(Collections.singletonList("tls:box.tail1234.ts.net:47321"), 40 * DAY);
        assertEquals(Collections.singletonList("tls:box.tail1234.ts.net:47321"), saved.routes());
    }

    @Test
    public void planRemembersPerNetworkAndSkipsTheLanOnCellular() {
        SavedRoutes saved = new SavedRoutes();
        saved.merge(Arrays.asList("tls:box.example.com:47321", "tls:192.168.1.20:47321", "relay:later", "tls:[fd00::7]:47321", "tls:100.101.102.103:47321"), 1000);
        SavedRoutes.Plan fresh = saved.plan(NetworkType.WIFI);
        assertNull(fresh.first);
        assertEquals(Arrays.asList(Route.parse("tls:box.example.com:47321"), Route.parse("tls:192.168.1.20:47321"),
                Route.parse("tls:[fd00::7]:47321"), Route.parse("tls:100.101.102.103:47321")), fresh.rest);

        saved.connected(Route.parse("tls:192.168.1.20:47321"), NetworkType.WIFI, 2000);
        saved.connected(Route.parse("tls:100.101.102.103:47321"), NetworkType.CELLULAR, 3000);
        assertEquals("tls:100.101.102.103:47321", saved.lastRoute());
        SavedRoutes.Plan wifi = saved.plan(NetworkType.WIFI);
        assertEquals(Route.parse("tls:192.168.1.20:47321"), wifi.first);
        assertEquals(3, wifi.rest.size());
        assertTrue(wifi.skipped.isEmpty());

        SavedRoutes.Plan cellular = saved.plan(NetworkType.CELLULAR);
        assertEquals(Route.parse("tls:100.101.102.103:47321"), cellular.first);
        assertEquals(Collections.singletonList(Route.parse("tls:box.example.com:47321")), cellular.rest);
        assertEquals(Arrays.asList(Route.parse("tls:192.168.1.20:47321"), Route.parse("tls:[fd00::7]:47321")), cellular.skipped);

        SavedRoutes.Plan other = saved.plan(NetworkType.OTHER);
        assertNull(other.first);
        assertEquals(4, other.rest.size());
        assertEquals("other", NetworkType.NONE.lastKey());
    }

    @Test
    public void aRememberedLanRouteIsSkippedOnCellularToo() {
        SavedRoutes saved = new SavedRoutes();
        saved.merge(Arrays.asList("tls:192.168.1.20:47321", "tls:box.example.com:47321"), 1000);
        saved.restoreLast("cellular", "tls:192.168.1.20:47321");
        SavedRoutes.Plan cellular = saved.plan(NetworkType.CELLULAR);
        assertNull(cellular.first);
        assertEquals(Collections.singletonList(Route.parse("tls:box.example.com:47321")), cellular.rest);
        assertEquals(1, cellular.skipped.size());
        assertTrue(Failures.summary(Collections.emptyList(), cellular.skipped, "box").contains("cellular"));
    }

    @Test
    public void onlyPrivateRoutesAreTriedOnCellularAnyway() {
        // A VPN over cellular reports as cellular and may reach the LAN.
        SavedRoutes saved = new SavedRoutes();
        saved.merge(Arrays.asList("tls:192.168.1.20:47321", "tls:[fd00::7]:47321", "tls:box.local:47321", "relay:later"), 1000);
        SavedRoutes.Plan fresh = saved.plan(NetworkType.CELLULAR);
        assertNull(fresh.first);
        assertEquals(3, fresh.rest.size());
        assertTrue(fresh.skipped.isEmpty());

        saved.restoreLast("cellular", "tls:[fd00::7]:47321");
        SavedRoutes.Plan remembered = saved.plan(NetworkType.CELLULAR);
        assertEquals(Route.parse("tls:[fd00::7]:47321"), remembered.first);
        assertEquals(Arrays.asList(Route.parse("tls:192.168.1.20:47321"), Route.parse("tls:box.local:47321")), remembered.rest);
        assertTrue(remembered.skipped.isEmpty());
    }

    @Test
    public void restoreCanonicalizesAndDropsWhatItCannotRead() {
        SavedRoutes saved = new SavedRoutes();
        saved.restore("tls:Box.Example.com:1", 5);
        saved.restore("tls:box.example.com:1", 6);
        saved.restore("garbage", 7);
        saved.restore("relay:x", 8);
        saved.prune();
        assertEquals(Arrays.asList("tls:box.example.com:1", "relay:x"), saved.routes());
    }

    @Test
    public void networkTypes() {
        assertEquals(NetworkType.NONE, NetworkType.of(false, true, true));
        assertEquals(NetworkType.WIFI, NetworkType.of(true, true, true));
        assertEquals(NetworkType.CELLULAR, NetworkType.of(true, false, true));
        assertEquals(NetworkType.OTHER, NetworkType.of(true, false, false));
        assertEquals("cellular", NetworkType.CELLULAR.wire);
    }
}
