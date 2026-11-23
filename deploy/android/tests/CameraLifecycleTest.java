package io.github.rusttalkcrew.rusttalk;

import java.util.concurrent.*;

/** Fake native callbacks; no device, camera image, or Android runtime is used. */
public final class CameraLifecycleTest {
    private static final ExecutorService OPERATIONS = Executors.newSingleThreadExecutor();
    private static final ExecutorService CALLERS = Executors.newCachedThreadPool();

    private static final class Backend implements CameraLifecycle.Backend {
        final CameraLifecycle.Lease lease;
        volatile boolean cancelled, front, light, available = true, failOpen, holdClose;
        volatile int opens, torchCalls, closes;
        volatile Object session;
        volatile CountDownLatch entered = new CountDownLatch(1);
        volatile CountDownLatch deliverOpen = new CountDownLatch(0);
        Backend(CameraLifecycle.Lease lease) { this.lease = lease; }
        public int state() { return session == null || cancelled ? 0 : front ? 1 : light ? 14 : 6; }
        public boolean available(boolean front) { return available; }
        public boolean open(boolean front) {
            Object next = new Object();
            if (!lease.acquire(next)) return false;
            session = next;
            opens++;
            entered.countDown();
            await(deliverOpen);
            if (cancelled) { closes++; return false; }
            if (failOpen) {
                failOpen = false;
                lease.closed(next);
                session = null;
                return false;
            }
            this.front = front;
            light = false;
            return true;
        }
        public boolean release() {
            closes++;
            light = false;
            if (holdClose) return false;
            acknowledgeClose();
            return true;
        }
        public boolean torch(boolean on) { torchCalls++; light = on; return true; }
        public void cancel() { cancelled = true; light = false; }
        void acknowledgeClose() {
            Object old = session;
            if (old != null) lease.closed(old);
            session = null;
        }
        void delayOpen() {
            entered = new CountDownLatch(1);
            deliverOpen = new CountDownLatch(1);
        }
    }

    private static void await(CountDownLatch latch) {
        try { assert latch.await(5, TimeUnit.SECONDS) : "test callback not delivered"; }
        catch (InterruptedException e) { throw new AssertionError(e); }
    }
    private static boolean result(Future<Boolean> result) throws Exception {
        return result.get(2, TimeUnit.SECONDS);
    }
    private static void drain() throws Exception {
        OPERATIONS.submit(() -> {}).get(5, TimeUnit.SECONDS);
    }
    private static CameraLifecycle camera(Backend backend) {
        return new CameraLifecycle(backend, OPERATIONS, 1000);
    }

    private static void pauseBeforeQueuedStart() throws Exception {
        Backend backend = new Backend(new CameraLifecycle.Lease());
        CountDownLatch queued = new CountDownLatch(1);
        java.util.concurrent.atomic.AtomicReference<Runnable> work = new java.util.concurrent.atomic.AtomicReference<>();
        CameraLifecycle camera = new CameraLifecycle(backend, task -> {
            work.set(task);
            queued.countDown();
        }, 1000);
        Future<Boolean> start = CALLERS.submit(() -> camera.start(true));
        await(queued);
        camera.cancel();
        assert !result(start);
        work.get().run();
        assert backend.opens == 0 : "paused queued start must never reach native open";
    }

    private static void successfulFlipKeepsControllerAndClearsTorch() {
        Backend backend = new Backend(new CameraLifecycle.Lease());
        CameraLifecycle camera = camera(backend);
        assert camera.start(false);
        assert camera.torch(true) && camera.state() == 14;
        assert camera.select(true) && camera.state() == 1 && !backend.light;
        assert camera.select(false) && camera.state() == 6 && !backend.light;
        assert backend.opens == 3 && backend.closes == 2;
        camera.cancel();
        backend.acknowledgeClose();
    }

    private static void delayedOpenThenPause() throws Exception {
        CameraLifecycle.Lease lease = new CameraLifecycle.Lease();
        Backend backend = new Backend(lease);
        backend.delayOpen();
        CameraLifecycle camera = camera(backend);
        Future<Boolean> start = CALLERS.submit(() -> camera.start(true));
        await(backend.entered);
        // Must return without the open callback and wake the JNI caller immediately.
        CALLERS.submit(camera::cancel).get(1, TimeUnit.SECONDS);
        assert !result(start);
        assert camera.ended() && camera.state() == 0;
        backend.deliverOpen.countDown();
        drain();
        assert backend.closes == 1 : "late device must close";
        Backend resumed = new Backend(lease);
        assert !camera(resumed).start(true) : "unacknowledged close must retain the lease";
        backend.acknowledgeClose();
        CameraLifecycle recovered = camera(resumed);
        assert recovered.start(true) : "resume after close must recover";
        recovered.cancel();
        resumed.acknowledgeClose();
    }

    private static void delayedFlipThenPause() throws Exception {
        CameraLifecycle.Lease lease = new CameraLifecycle.Lease();
        Backend backend = new Backend(lease);
        CameraLifecycle camera = camera(backend);
        assert camera.start(false);
        assert camera.torch(true) && camera.state() == 14;
        backend.delayOpen();
        Future<Boolean> flip = CALLERS.submit(() -> camera.select(true));
        await(backend.entered);
        assert !backend.light : "switch must clear torch";
        CALLERS.submit(camera::cancel).get(1, TimeUnit.SECONDS);
        assert !result(flip);
        assert camera.state() == 0 && !backend.light;
        assert !camera.torch(true);
        backend.deliverOpen.countDown();
        drain();
        assert backend.opens == 2 : "pause must suppress rollback/reopening";
        assert backend.torchCalls == 1 : "paused torch command must not run";
        backend.acknowledgeClose();
        Backend resumed = new Backend(lease);
        CameraLifecycle recovered = camera(resumed);
        assert recovered.start(false) && recovered.state() == 6;
        recovered.cancel();
        resumed.acknowledgeClose();
    }

    private static void missingCloseAndCommandTimeout() throws Exception {
        CameraLifecycle.Lease lease = new CameraLifecycle.Lease();
        Backend backend = new Backend(lease);
        CameraLifecycle camera = camera(backend);
        assert camera.start(false);
        assert camera.torch(true);
        backend.holdClose = true;
        assert !camera.select(true);
        assert camera.ended() && !backend.light;
        Backend resumed = new Backend(lease);
        assert !camera(resumed).start(true) : "close timeout must not permit overlap";
        Object stale = backend.session;
        backend.acknowledgeClose();
        resumed.delayOpen();
        CameraLifecycle timeout = new CameraLifecycle(resumed, OPERATIONS, 25);
        Future<Boolean> start = CALLERS.submit(() -> timeout.start(true));
        await(resumed.entered);
        assert !result(start) : "missing callback must have a bounded caller wait";
        assert timeout.ended();
        resumed.deliverOpen.countDown();
        drain();
        resumed.acknowledgeClose();
        Backend retry = new Backend(lease);
        CameraLifecycle recovered = camera(retry);
        assert recovered.start(true);
        // A stale onClosed from the old device cannot release the replacement's lease.
        lease.closed(stale);
        assert !lease.acquire(new Object());
        recovered.cancel();
        retry.acknowledgeClose();
    }

    private static void deniedSwitchAndRollback() {
        Backend backend = new Backend(new CameraLifecycle.Lease());
        CameraLifecycle camera = camera(backend);
        assert camera.start(false);
        Object original = backend.session;
        assert camera.torch(true);
        backend.available = false;
        assert !camera.select(true);
        assert backend.session == original && camera.state() == 14 && backend.closes == 0;
        backend.available = true;
        backend.failOpen = true;
        assert !camera.select(true);
        assert camera.state() == 6 && backend.opens == 3 : "failed switch must restore rear capture";
        camera.cancel();
        backend.acknowledgeClose();
    }

    public static void main(String[] args) throws Exception {
        try {
            pauseBeforeQueuedStart();
            successfulFlipKeepsControllerAndClearsTorch();
            delayedOpenThenPause();
            delayedFlipThenPause();
            missingCloseAndCommandTimeout();
            deniedSwitchAndRollback();
            System.out.println("PASS: delayed open/flip + pause, bounded timeout, close lease, resume, torch and rollback");
        } finally {
            OPERATIONS.shutdownNow();
            CALLERS.shutdownNow();
        }
    }
}
