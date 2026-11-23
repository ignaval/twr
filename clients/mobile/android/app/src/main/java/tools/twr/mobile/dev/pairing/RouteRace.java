package tools.twr.mobile.dev.pairing;

import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Executor;

/**
 * Races a hub's routes (docs/design/phone-pairing.md, Racing routes): the plan's first route
 * at once, the rest after a head start (or as soon as the first fails), in parallel. The first
 * probe to return wins and every other attempt is cancelled. The probe does the pin check and
 * the {@code /health} gateway check, so winning means both passed.
 */
public final class RouteRace<T> {
    /** One attempt on one route; blocking. Register cancellation with {@link Attempt#onCancel}. */
    public interface Probe<T> {
        T attempt(Route route, Attempt attempt) throws Exception;
    }

    /** Lets a probe abort its blocking I/O when the race no longer needs it. */
    public static final class Attempt {
        private final List<Runnable> hooks = new ArrayList<>();
        private boolean cancelled;

        public void onCancel(Runnable hook) {
            boolean now;
            synchronized (this) {
                now = cancelled;
                if (!now) hooks.add(hook);
            }
            if (now) hook.run();
        }

        public synchronized boolean cancelled() {
            return cancelled;
        }

        void cancel() {
            List<Runnable> run;
            synchronized (this) {
                if (cancelled) return;
                cancelled = true;
                run = new ArrayList<>(hooks);
                hooks.clear();
            }
            for (Runnable hook : run) {
                try {
                    hook.run();
                } catch (RuntimeException ignored) {
                    // Cancelling is best effort; the attempt's own failure is what gets reported.
                }
            }
        }
    }

    public static final class Won<T> {
        public final Route route;
        public final T value;

        Won(Route route, T value) {
            this.route = route;
            this.value = value;
        }
    }

    /** Why one route lost. */
    public static final class Failure {
        public final Route route;
        public final Throwable error;

        Failure(Route route, Throwable error) {
            this.route = route;
            this.error = error;
        }
    }

    /** No route won. {@link #failures} has every dialed route's reason, in the plan's order. */
    public static final class Failed extends Exception {
        public final List<Failure> failures;
        public final List<Route> skipped;
        public final boolean cancelled;

        Failed(List<Failure> failures, List<Route> skipped, boolean cancelled) {
            super(cancelled ? "cancelled" : "no route answered");
            this.failures = Collections.unmodifiableList(new ArrayList<>(failures));
            this.skipped = skipped;
            this.cancelled = cancelled;
        }
    }

    /** Raised for a route still pending at the race's deadline. */
    public static final class DeadlineExceeded extends java.io.InterruptedIOException {
        DeadlineExceeded() {
            super("timed out");
        }
    }

    private final Executor executor;
    private final Probe<T> probe;
    private final Object lock = new Object();
    private final List<Attempt> attempts = new ArrayList<>();
    private final List<Route> started = new ArrayList<>();
    private final List<Failure> failures = new ArrayList<>();
    private Won<T> winner;
    private Attempt winning;
    private int pending;
    private boolean firstFailed;
    private boolean cancelled;

    public RouteRace(Executor executor, Probe<T> probe) {
        this.executor = executor;
        this.probe = probe;
    }

    /** Cancels the race from another thread: {@link #run} throws {@link Failed} with {@code cancelled}. */
    public void cancel() {
        synchronized (lock) {
            cancelled = true;
            lock.notifyAll();
        }
        cancelLosers();
    }

    /** Runs the race; blocks until a route wins, every route fails, the deadline passes or it is cancelled. */
    public Won<T> run(SavedRoutes.Plan plan, long headStartMillis, long deadlineMillis) throws Failed, InterruptedException {
        long deadline = System.nanoTime() + deadlineMillis * 1_000_000L;
        try {
            if (plan.first != null) {
                start(plan.first, true);
                long headStartEnd = System.nanoTime() + headStartMillis * 1_000_000L;
                synchronized (lock) {
                    while (winner == null && !cancelled && !firstFailed) {
                        long wait = (headStartEnd - System.nanoTime()) / 1_000_000L;
                        if (wait <= 0) break;
                        lock.wait(wait);
                    }
                }
            }
            for (Route route : plan.rest) {
                synchronized (lock) {
                    if (winner != null || cancelled) break;
                }
                start(route, false);
            }
            synchronized (lock) {
                while (winner == null && !cancelled && pending > 0) {
                    long wait = (deadline - System.nanoTime()) / 1_000_000L;
                    if (wait <= 0) break;
                    lock.wait(wait);
                }
                if (winner != null) return winner;
                if (!cancelled && pending > 0) {
                    // Still pending at the deadline: those routes timed out.
                    for (Route route : started) {
                        if (!failed(route)) failures.add(new Failure(route, new DeadlineExceeded()));
                    }
                }
                throw new Failed(ordered(plan), plan.skipped, cancelled);
            }
        } catch (InterruptedException interrupted) {
            synchronized (lock) {
                cancelled = true;
            }
            throw interrupted;
        } finally {
            cancelLosers();
        }
    }

    private boolean failed(Route route) {
        for (Failure failure : failures) if (failure.route.equals(route)) return true;
        return false;
    }

    private List<Failure> ordered(SavedRoutes.Plan plan) {
        List<Route> order = new ArrayList<>();
        if (plan.first != null) order.add(plan.first);
        order.addAll(plan.rest);
        List<Failure> sorted = new ArrayList<>();
        for (Route route : order) {
            for (Failure failure : failures) if (failure.route.equals(route)) sorted.add(failure);
        }
        return sorted;
    }

    private void start(final Route route, final boolean first) {
        final Attempt attempt = new Attempt();
        synchronized (lock) {
            if (cancelled) return;
            attempts.add(attempt);
            started.add(route);
            pending++;
        }
        executor.execute(() -> {
            T value = null;
            Throwable error = null;
            try {
                value = probe.attempt(route, attempt);
            } catch (Throwable failure) {
                error = failure;
            }
            synchronized (lock) {
                pending--;
                if (error == null && winner == null && !cancelled) {
                    winner = new Won<>(route, value);
                    winning = attempt;
                } else if (error != null && !failed(route)) {
                    failures.add(new Failure(route, error));
                    if (first) firstFailed = true;
                }
                lock.notifyAll();
            }
        });
    }

    /** Cancels every attempt but the winner's: called once the race is decided or abandoned. */
    private void cancelLosers() {
        List<Attempt> losers = new ArrayList<>();
        synchronized (lock) {
            for (Attempt attempt : attempts) if (attempt != winning) losers.add(attempt);
        }
        for (Attempt attempt : losers) attempt.cancel();
    }
}
