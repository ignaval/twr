package tools.twr.mobile.dev.pairing;

import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * A pinned hub's known routes (docs/design/phone-pairing.md, Saved hub): at most 16,
 * deduplicated in canonical form, each dropped after 30 days unseen; and per network type the
 * route that last connected. Unknown kinds are kept and never dialed. The plugin stores this in
 * the encrypted vault; this class holds no Android or JSON types.
 */
public final class SavedRoutes {
    public static final int MAX_ROUTES = 16;
    public static final long EXPIRY_MILLIS = 30L * 24 * 60 * 60 * 1000;

    /** A route and when it was last reported by the hub or connected (epoch milliseconds). */
    public static final class Entry {
        public final String route;
        public final long seen;

        Entry(String route, long seen) {
            this.route = route;
            this.seen = seen;
        }
    }

    private final List<Entry> entries = new ArrayList<>();
    private final Map<String, String> last = new LinkedHashMap<>();
    private String lastRoute;

    /**
     * Adds or refreshes routes. Invalid entries are skipped and returned; valid ones are stored
     * canonically. Then the list is pruned to the limits.
     */
    public List<String> merge(Collection<String> reported, long now) {
        List<String> rejected = new ArrayList<>();
        if (reported != null) {
            for (String text : reported) {
                Route route;
                try {
                    route = Route.parse(text);
                } catch (IllegalArgumentException invalid) {
                    rejected.add(text);
                    continue;
                }
                see(route.toString(), now);
            }
        }
        prune();
        return rejected;
    }

    /** Restores one stored entry (from the vault); an invalid one is dropped. */
    public void restore(String text, long seen) {
        try {
            String canonical = Route.parse(text).toString();
            for (Entry entry : entries) if (entry.route.equals(canonical)) return;
            entries.add(new Entry(canonical, seen));
        } catch (IllegalArgumentException invalid) {
            // Not a route this version can read: nothing to keep.
        }
    }

    /** Restores the saved {@code last} map and overall last route; call after {@link #restore} and then {@link #prune}. */
    public void restoreLast(String network, String route) {
        if (network != null && route != null) last.put(network, route);
    }

    public void restoreLastRoute(String route) {
        lastRoute = route;
    }

    /** {@code winner} connected on {@code network}: it becomes {@code last} there and is seen now. */
    public void connected(Route winner, NetworkType network, long now) {
        see(winner.toString(), now);
        last.put(network.lastKey(), winner.toString());
        lastRoute = winner.toString();
        prune();
    }

    private void see(String canonical, long now) {
        for (int i = 0; i < entries.size(); i++) {
            Entry entry = entries.get(i);
            if (entry.route.equals(canonical)) {
                entries.set(i, new Entry(canonical, Math.max(entry.seen, now)));
                return;
            }
        }
        entries.add(new Entry(canonical, now));
    }

    /**
     * Drops routes unseen for 30 days, then keeps the 16 seen most recently (in their order).
     * "Unseen" counts from the newest sighting of any route, the last time the app heard from
     * the hub, so a phone that stayed away for a month still knows where to look.
     */
    public void prune() {
        long newest = Long.MIN_VALUE;
        for (Entry entry : entries) newest = Math.max(newest, entry.seen);
        for (Iterator<Entry> it = entries.iterator(); it.hasNext(); ) {
            if (it.next().seen < newest - EXPIRY_MILLIS) it.remove();
        }
        while (entries.size() > MAX_ROUTES) {
            int oldest = 0;
            // The least recently seen goes; among equals, the one listed last.
            for (int i = 1; i < entries.size(); i++) if (entries.get(i).seen <= entries.get(oldest).seen) oldest = i;
            entries.remove(oldest);
        }
        for (Iterator<Map.Entry<String, String>> it = last.entrySet().iterator(); it.hasNext(); ) {
            if (!contains(it.next().getValue())) it.remove();
        }
        if (lastRoute != null && !contains(lastRoute)) lastRoute = null;
    }

    private boolean contains(String route) {
        for (Entry entry : entries) if (entry.route.equals(route)) return true;
        return false;
    }

    public List<Entry> entries() {
        return Collections.unmodifiableList(entries);
    }

    public List<String> routes() {
        List<String> routes = new ArrayList<>(entries.size());
        for (Entry entry : entries) routes.add(entry.route);
        return routes;
    }

    public Map<String, String> lastByNetwork() {
        return Collections.unmodifiableMap(last);
    }

    /** The route that last connected on any network, or null. */
    public String lastRoute() {
        return lastRoute;
    }

    /** What to dial, and in what order. */
    public static final class Plan {
        /** Dialed at once; null when nothing is remembered for this network, and then {@link #rest} starts at once. */
        public final Route first;
        /** Dialed after the head start, in parallel. */
        public final List<Route> rest;
        /** Dialable routes left out on this network (private LAN on cellular). */
        public final List<Route> skipped;

        Plan(Route first, List<Route> rest, List<Route> skipped) {
            this.first = first;
            this.rest = Collections.unmodifiableList(rest);
            this.skipped = Collections.unmodifiableList(skipped);
        }

        public boolean empty() {
            return first == null && rest.isEmpty();
        }
    }

    /**
     * The routes to race on {@code network} (docs/design/phone-pairing.md, Racing routes): the
     * remembered one first, then the remaining {@code tls} routes; on cellular, private LAN
     * addresses are skipped.
     */
    public Plan plan(NetworkType network) {
        List<Route> dialable = new ArrayList<>();
        List<Route> skipped = new ArrayList<>();
        for (Entry entry : entries) {
            Route route = Route.parse(entry.route);
            if (!route.dialable()) continue;
            if (network == NetworkType.CELLULAR && route.privateLan()) skipped.add(route);
            else dialable.add(route);
        }
        Route first = null;
        String remembered = last.get(network.lastKey());
        for (Route route : dialable) {
            if (route.toString().equals(remembered)) first = route;
        }
        if (first != null) dialable.remove(first);
        return new Plan(first, dialable, skipped);
    }
}
