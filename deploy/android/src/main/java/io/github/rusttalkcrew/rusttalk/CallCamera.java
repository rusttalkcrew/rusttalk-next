package io.github.rusttalkcrew.rusttalk;

import android.app.Activity;
import android.content.Context;
import android.content.pm.PackageManager;
import android.graphics.ImageFormat;
import android.hardware.camera2.*;
import android.media.Image;
import android.media.ImageReader;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Size;
import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.Collections;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import livekit.org.webrtc.YuvHelper;

/** One native session feeds both the encrypted track and the in-memory preview. */
final class CallCamera {
    private static final Handler HANDLER;
    // A timed-out close keeps this lease until onClosed. Never overlap native devices.
    private static final CameraLifecycle.Lease LEASE = new CameraLifecycle.Lease();
    static {
        HandlerThread thread = new HandlerThread("RustTalk camera callbacks");
        thread.start();
        HANDLER = new Handler(thread.getLooper());
    }
    private final Activity activity;
    private CameraManager manager;
    private volatile Session active;
    private final int width, height, fps;
    private final long generation;
    private final CameraLifecycle lifecycle;

    private static native void frame(ByteBuffer pixels, int width, int height, long generation);

    private final class Session {
        final String id;
        final CameraCharacteristics info;
        final CountDownLatch ready = new CountDownLatch(1);
        final CountDownLatch closed = new CountDownLatch(1);
        volatile boolean dead, configured, torchOn;
        volatile boolean opening;
        final ByteBuffer[] packed = new ByteBuffer[3];
        volatile CameraDevice device;
        volatile CameraCaptureSession capture;
        volatile ImageReader reader;
        volatile CaptureRequest.Builder request;
        ByteBuffer pixels;
        Session(String id) throws CameraAccessException {
            this.id = id;
            info = manager.getCameraCharacteristics(id);
        }
        boolean front() { return Integer.valueOf(CameraCharacteristics.LENS_FACING_FRONT)
            .equals(info.get(CameraCharacteristics.LENS_FACING)); }
        boolean light() { return !front() && Boolean.TRUE.equals(info.get(CameraCharacteristics.FLASH_INFO_AVAILABLE)); }
    }

    CallCamera(Activity activity, int width, int height, int fps, long generation) {
        this.activity = activity;
        this.width = width;
        this.height = height;
        this.fps = fps;
        this.generation = generation;
        lifecycle = new CameraLifecycle(new CameraLifecycle.Backend() {
            public int state() { return nativeState(); }
            public boolean available(boolean front) {
                try {
                    return activity.checkSelfPermission(android.Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
                        && find(front) != null;
                } catch (CameraAccessException e) { return false; }
            }
            public boolean open(boolean front) { return openDevice(front); }
            public boolean release() { return releaseDevice(active); }
            public boolean torch(boolean on) { return nativeTorch(on); }
            public void cancel() { cancelDevice(); }
        });
    }

    boolean start(boolean front) { return lifecycle.start(front); }
    boolean select(boolean front) { return lifecycle.select(front); }
    boolean torch(boolean on) { return lifecycle.torch(on); }
    int state() { return lifecycle.state(); }
    boolean ended() { return lifecycle.ended() || (active != null && active.dead); }
    void cancel() { lifecycle.cancel(); }

    private String find(boolean front) throws CameraAccessException {
        for (String id : manager.getCameraIdList()) {
            Integer facing = manager.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING);
            if (Integer.valueOf(front ? CameraCharacteristics.LENS_FACING_FRONT :
                    CameraCharacteristics.LENS_FACING_BACK).equals(facing)) return id;
        }
        return null;
    }

    private boolean openDevice(boolean front) {
        Session s = null;
        boolean accepted = false;
        try {
            if (lifecycle.cancelled()) return false;
            manager = (CameraManager) activity.getSystemService(Context.CAMERA_SERVICE);
            String id = find(front);
            if (id == null) return false;
            s = new Session(id);
            if (!LEASE.acquire(s)) return false;
            synchronized (this) {
                if (lifecycle.cancelled()) { LEASE.closed(s); return false; }
                active = s;
            }
            Session target = s;
            Size[] sizes = s.info.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
                .getOutputSizes(ImageFormat.YUV_420_888);
            Size size = Arrays.stream(sizes).min(java.util.Comparator.comparingLong(v ->
                Math.abs((long)v.getWidth() * v.getHeight() - (long)width * height))).orElseThrow();
            s.reader = ImageReader.newInstance(size.getWidth(), size.getHeight(), ImageFormat.YUV_420_888, 2);
            s.reader.setOnImageAvailableListener(reader -> receive(target), HANDLER);
            if (lifecycle.cancelled()) {
                closeNative(s);
                LEASE.closed(s);
                return false;
            }
            s.opening = true;
            manager.openCamera(id, new CameraDevice.StateCallback() {
                @Override public void onOpened(CameraDevice device) {
                    target.device = device;
                    if (target.dead || lifecycle.cancelled()) { closeNative(target); return; }
                    try {
                        target.request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD);
                        target.request.addTarget(target.reader.getSurface());
                        target.request.set(CaptureRequest.CONTROL_AE_MODE, CaptureRequest.CONTROL_AE_MODE_ON);
                        target.request.set(CaptureRequest.FLASH_MODE, CaptureRequest.FLASH_MODE_OFF);
                        android.util.Range<Integer>[] ranges = target.info.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES);
                        if (ranges != null) Arrays.stream(ranges).min(java.util.Comparator.comparingInt(r ->
                            Math.abs(r.getUpper() - fps) * 100 + Math.abs(r.getLower() - fps)))
                            .ifPresent(r -> target.request.set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, r));
                        device.createCaptureSession(Collections.singletonList(target.reader.getSurface()),
                            new CameraCaptureSession.StateCallback() {
                                @Override public void onConfigured(CameraCaptureSession capture) {
                                    if (target.dead || lifecycle.cancelled()) { capture.close(); closeNative(target); return; }
                                    target.capture = capture;
                                    try {
                                        repeat(target);
                                        target.configured = true;
                                    } catch (CameraAccessException | IllegalStateException e) { target.dead = true; }
                                    target.ready.countDown();
                                }
                                @Override public void onConfigureFailed(CameraCaptureSession capture) {
                                    capture.close();
                                    target.dead = true;
                                    target.ready.countDown();
                                }
                            }, HANDLER);
                    } catch (CameraAccessException | IllegalStateException e) {
                        target.dead = true;
                        target.ready.countDown();
                    }
                }
                @Override public void onDisconnected(CameraDevice device) { fail(device); }
                @Override public void onError(CameraDevice device, int error) { fail(device); }
                private void fail(CameraDevice device) {
                    boolean interrupted = target.configured && !target.dead;
                    target.dead = true;
                    target.torchOn = false;
                    target.ready.countDown();
                    device.close();
                    if (active == target && interrupted) lifecycle.cancel();
                }
                @Override public void onClosed(CameraDevice device) {
                    target.dead = true;
                    target.torchOn = false;
                    target.ready.countDown();
                    LEASE.closed(target);
                    target.closed.countDown();
                }
            }, HANDLER);
            accepted = true;
            if (s.ready.await(4, TimeUnit.SECONDS) && s.configured && !s.dead && !lifecycle.cancelled()) return true;
        } catch (Exception e) {
            android.util.Log.w("RustTalk", "Camera could not open", e);
            if (s != null && !accepted) {
                // openCamera threw before accepting the device; no onClosed is due.
                s.opening = false;
                LEASE.closed(s);
            }
        }
        releaseDevice(s);
        return false;
    }

    private void repeat(Session s) throws CameraAccessException {
        s.capture.setRepeatingRequest(s.request.build(), new CameraCaptureSession.CaptureCallback() {
            @Override public void onCaptureCompleted(CameraCaptureSession session, CaptureRequest request,
                    TotalCaptureResult result) {
                Integer flash = result.get(CaptureResult.FLASH_STATE);
                s.torchOn = !s.dead && Integer.valueOf(CaptureResult.FLASH_STATE_FIRED).equals(flash)
                    && Integer.valueOf(CaptureRequest.FLASH_MODE_TORCH).equals(request.get(CaptureRequest.FLASH_MODE));
            }
        }, HANDLER);
    }

    private int nativeState() {
        Session s = active;
        if (lifecycle.cancelled() || s == null || s.dead || !s.configured) return 0;
        return (s.front() ? 1 : 2) | (s.light() ? 4 : 0) | (s.torchOn ? 8 : 0);
    }

    private boolean nativeTorch(boolean on) {
        Session s = active;
        if (s == null || !s.light() || lifecycle.cancelled()) return false;
        java.util.concurrent.CompletableFuture<Boolean> result = new java.util.concurrent.CompletableFuture<>();
        HANDLER.post(() -> {
            if (s.dead || lifecycle.cancelled()) { result.complete(false); return; }
            try {
                s.request.set(CaptureRequest.FLASH_MODE, on ? CaptureRequest.FLASH_MODE_TORCH : CaptureRequest.FLASH_MODE_OFF);
                repeat(s);
                result.complete(true);
            } catch (CameraAccessException | IllegalStateException e) {
                android.util.Log.w("RustTalk", "Camera light unavailable", e);
                result.complete(false);
            }
        });
        try { return result.get(1, TimeUnit.SECONDS); }
        catch (Exception e) { lifecycle.cancel(); return false; }
    }

    private void cancelDevice() {
        Session s;
        synchronized (this) { s = active; }
        if (s != null) {
            s.dead = true;
            s.torchOn = false;
            s.ready.countDown();
            HANDLER.post(() -> closeNative(s));
        }
    }

    private boolean releaseDevice(Session s) {
        if (s == null) return true;
        s.dead = true;
        s.torchOn = false;
        s.ready.countDown();
        CountDownLatch stopped = new CountDownLatch(1);
        HANDLER.post(() -> {
            try { closeNative(s); } finally { stopped.countDown(); }
        });
        try {
            return stopped.await(1, TimeUnit.SECONDS)
                && (!s.opening || s.closed.await(1, TimeUnit.SECONDS));
        } catch (InterruptedException e) { Thread.currentThread().interrupt(); return false; }
    }

    private void closeNative(Session s) {
        s.dead = true;
        s.torchOn = false;
        s.ready.countDown();
        if (s.capture != null) {
            try {
                s.request.set(CaptureRequest.FLASH_MODE, CaptureRequest.FLASH_MODE_OFF);
                s.capture.capture(s.request.build(), null, HANDLER);
                s.capture.stopRepeating();
            } catch (CameraAccessException | IllegalStateException e) {
                android.util.Log.w("RustTalk", "Closing unavailable camera", e);
            }
            s.capture.close();
        }
        if (s.device != null) s.device.close();
        if (s.reader != null) s.reader.close();
    }

    private void receive(Session s) {
        try (Image image = s.reader.acquireLatestImage()) {
            if (image == null || lifecycle.cancelled() || s.dead || active != s) return;
            int w = image.getWidth(), h = image.getHeight();
            Image.Plane[] planes = image.getPlanes();
            ByteBuffer[] packed = s.packed;
            for (int p = 0; p < 3; p++) {
                int pw = p == 0 ? w : (w + 1) / 2, ph = p == 0 ? h : (h + 1) / 2;
                if (packed[p] == null || packed[p].capacity() != pw * ph)
                    packed[p] = ByteBuffer.allocateDirect(pw * ph);
                packed[p].clear();
                ByteBuffer source = planes[p].getBuffer();
                if (planes[p].getPixelStride() == 1) {
                    YuvHelper.copyPlane(source.slice(), planes[p].getRowStride(), packed[p], pw, pw, ph);
                } else {
                    int base = source.position();
                    for (int y = 0; y < ph; y++) for (int x = 0; x < pw; x++)
                        packed[p].put(source.get(base + y * planes[p].getRowStride() + x * planes[p].getPixelStride()));
                    packed[p].rewind();
                }
            }
            int display = activity.getWindowManager().getDefaultDisplay().getRotation() * 90;
            int sensor = s.info.get(CameraCharacteristics.SENSOR_ORIENTATION);
            int rotation = (sensor + (s.front() ? display : -display) + 360) % 360;
            int rw = rotation % 180 == 0 ? w : h, rh = rotation % 180 == 0 ? h : w;
            int bytes = rw * rh + 2 * ((rw + 1) / 2) * ((rh + 1) / 2);
            if (s.pixels == null || s.pixels.capacity() != bytes) s.pixels = ByteBuffer.allocateDirect(bytes);
            YuvHelper.I420Rotate(packed[0], w, packed[1], (w + 1) / 2, packed[2], (w + 1) / 2,
                s.pixels, w, h, rotation);
            if (!lifecycle.cancelled() && !s.dead) frame(s.pixels, rw, rh, generation);
        } catch (IllegalStateException e) {
            if (!s.dead) { lifecycle.cancel(); android.util.Log.w("RustTalk", "Camera frame unavailable", e); }
        }
    }
}
