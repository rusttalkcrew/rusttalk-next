package io.github.rusttalkcrew.rusttalk;

import java.util.concurrent.CompletableFuture;
import java.util.concurrent.Executor;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.BooleanSupplier;

/** Serial camera commands; lifecycle invalidation never waits for the command queue. */
final class CameraLifecycle {
    static final class Lease {
        private final java.util.concurrent.atomic.AtomicReference<Object> device =
            new java.util.concurrent.atomic.AtomicReference<>();
        boolean acquire(Object session) { return device.compareAndSet(null, session); }
        void closed(Object session) { device.compareAndSet(session, null); }
    }

    interface Backend {
        int state();
        boolean available(boolean front);
        boolean open(boolean front);
        boolean release();
        boolean torch(boolean on);
        // Must only invalidate and enqueue native cleanup, never wait or call the device.
        void cancel();
    }

    private static final Executor OPERATIONS = Executors.newSingleThreadExecutor(r -> {
        Thread thread = new Thread(r, "RustTalk camera operations");
        thread.setDaemon(true);
        return thread;
    });
    private final Backend backend;
    private final Executor operations;
    private final long timeoutMillis;
    private final AtomicBoolean cancelled = new AtomicBoolean();
    private volatile boolean ended;
    private CompletableFuture<Boolean> pending;

    CameraLifecycle(Backend backend) { this(backend, OPERATIONS, 5000); }
    CameraLifecycle(Backend backend, Executor operations, long timeoutMillis) {
        this.backend = backend;
        this.operations = operations;
        this.timeoutMillis = timeoutMillis;
    }

    boolean cancelled() { return cancelled.get(); }
    boolean ended() { return ended || cancelled(); }
    int state() { return ended() ? 0 : backend.state(); }

    boolean start(boolean front) {
        return command(() -> {
            boolean opened = !cancelled() && backend.open(front);
            ended = !opened;
            return opened;
        });
    }

    boolean select(boolean front) {
        return command(() -> {
            int state = state();
            if (state == 0) return false;
            boolean wasFront = (state & 1) != 0;
            if (wasFront == front) return true;
            if (!backend.available(front) || cancelled()) return false;
            if (!backend.release()) {
                cancel();
                return false;
            }
            if (cancelled()) return false;
            boolean changed = backend.open(front);
            // Never reopen the old camera after pause, timeout or hangup.
            if (!changed && !cancelled()) backend.open(wasFront);
            ended = backend.state() == 0;
            return changed && !cancelled();
        });
    }

    boolean torch(boolean on) {
        return command(() -> !ended() && backend.torch(on));
    }

    void cancel() {
        cancelled.set(true);
        synchronized (this) {
            if (pending != null) pending.complete(false);
        }
        backend.cancel();
    }

    private boolean command(BooleanSupplier action) {
        CompletableFuture<Boolean> result = new CompletableFuture<>();
        synchronized (this) {
            if (ended() || pending != null) return false;
            pending = result;
        }
        operations.execute(() -> {
            boolean success = false;
            try {
                success = !cancelled() && action.getAsBoolean() && !cancelled();
            } catch (RuntimeException e) {
                System.err.println("RustTalk camera operation failed: " + e.getClass().getSimpleName());
                cancel();
            } finally {
                synchronized (this) { if (pending == result) pending = null; }
                result.complete(success);
            }
        });
        try {
            return result.get(timeoutMillis, TimeUnit.MILLISECONDS) && !cancelled();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            cancel();
        } catch (java.util.concurrent.ExecutionException | java.util.concurrent.TimeoutException e) {
            cancel();
        }
        return false;
    }
}
