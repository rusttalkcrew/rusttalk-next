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
    private Activity activity;
    private CameraManager manager;
    private HandlerThread thread;
    private Handler handler;
    private volatile Session active;
    volatile boolean ended;
    private int width, height, fps;

    private static native void frame(ByteBuffer pixels, int width, int height);

    private final class Session {
        final String id;
        final CameraCharacteristics info;
        final CountDownLatch ready = new CountDownLatch(1);
        final CountDownLatch closed = new CountDownLatch(1);
        volatile boolean dead, configured, torchOn;
        boolean opening;
        final ByteBuffer[] packed = new ByteBuffer[3];
        CameraDevice device;
        CameraCaptureSession capture;
        ImageReader reader;
        CaptureRequest.Builder request;
        ByteBuffer pixels;
        Session(String id) throws CameraAccessException {
            this.id = id;
            info = manager.getCameraCharacteristics(id);
        }
        boolean front() { return Integer.valueOf(CameraCharacteristics.LENS_FACING_FRONT)
            .equals(info.get(CameraCharacteristics.LENS_FACING)); }
        boolean light() { return !front() && Boolean.TRUE.equals(info.get(CameraCharacteristics.FLASH_INFO_AVAILABLE)); }
    }

    boolean start(Activity activity, boolean front, int width, int height, int fps) {
        this.activity = activity;
        this.width = width;
        this.height = height;
        this.fps = fps;
        manager = (CameraManager) activity.getSystemService(Context.CAMERA_SERVICE);
        thread = new HandlerThread("RustTalk camera");
        thread.start();
        handler = new Handler(thread.getLooper());
        active = open(front);
        ended = active == null;
        return !ended;
    }

    private String find(boolean front) throws CameraAccessException {
        for (String id : manager.getCameraIdList()) {
            Integer facing = manager.getCameraCharacteristics(id).get(CameraCharacteristics.LENS_FACING);
            if (Integer.valueOf(front ? CameraCharacteristics.LENS_FACING_FRONT :
                    CameraCharacteristics.LENS_FACING_BACK).equals(facing)) return id;
        }
        return null;
    }

    private Session open(boolean front) {
        Session s = null;
        try {
            String id = find(front);
            if (id == null) return null;
            s = new Session(id);
            Session target = s;
            Size[] sizes = s.info.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP)
                .getOutputSizes(ImageFormat.YUV_420_888);
            Size size = Arrays.stream(sizes).min(java.util.Comparator.comparingLong(v ->
                Math.abs((long)v.getWidth() * v.getHeight() - (long)width * height))).orElseThrow();
            s.reader = ImageReader.newInstance(size.getWidth(), size.getHeight(), ImageFormat.YUV_420_888, 2);
            s.reader.setOnImageAvailableListener(reader -> receive(target), handler);
            manager.openCamera(id, new CameraDevice.StateCallback() {
                @Override public void onOpened(CameraDevice device) {
                    target.device = device;
                    if (target.dead) { device.close(); return; }
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
                                    if (target.dead) { capture.close(); return; }
                                    target.capture = capture;
                                    try {
                                        repeat(target);
                                        target.configured = true;
                                    } catch (CameraAccessException | IllegalStateException e) { target.dead = true; }
                                    target.ready.countDown();
                                }
                                @Override public void onConfigureFailed(CameraCaptureSession capture) {
                                    target.dead = true;
                                    target.ready.countDown();
                                }
                            }, handler);
                    } catch (CameraAccessException | IllegalStateException e) {
                        target.dead = true;
                        target.ready.countDown();
                    }
                }
                @Override public void onDisconnected(CameraDevice device) { fail(device); }
                @Override public void onError(CameraDevice device, int error) { fail(device); }
                private void fail(CameraDevice device) {
                    target.dead = true;
                    target.torchOn = false;
                    target.ready.countDown();
                    device.close();
                    if (active == target) ended = true;
                }
                @Override public void onClosed(CameraDevice device) { target.closed.countDown(); }
            }, handler);
            s.opening = true;
            if (s.ready.await(5, TimeUnit.SECONDS) && s.configured && !s.dead) return s;
        } catch (Exception e) {
            android.util.Log.w("RustTalk", "Camera could not open", e);
        }
        release(s);
        return null;
    }

    private void repeat(Session s) throws CameraAccessException {
        s.capture.setRepeatingRequest(s.request.build(), new CameraCaptureSession.CaptureCallback() {
            @Override public void onCaptureCompleted(CameraCaptureSession session, CaptureRequest request,
                    TotalCaptureResult result) {
                Integer flash = result.get(CaptureResult.FLASH_STATE);
                s.torchOn = !s.dead && Integer.valueOf(CaptureResult.FLASH_STATE_FIRED).equals(flash)
                    && Integer.valueOf(CaptureRequest.FLASH_MODE_TORCH).equals(request.get(CaptureRequest.FLASH_MODE));
            }
        }, handler);
    }

    int state() {
        Session s = active;
        if (ended || s == null || s.dead) return 0;
        return (s.front() ? 1 : 2) | (s.light() ? 4 : 0) | (s.torchOn ? 8 : 0);
    }

    boolean torch(boolean on) {
        Session s = active;
        if (ended || s == null || s.dead || !s.light()) return false;
        try {
            s.request.set(CaptureRequest.FLASH_MODE, on ? CaptureRequest.FLASH_MODE_TORCH : CaptureRequest.FLASH_MODE_OFF);
            repeat(s);
            return true;
        } catch (CameraAccessException | IllegalStateException e) {
            android.util.Log.w("RustTalk", "Camera light unavailable", e);
            return false;
        }
    }

    boolean select(boolean front) {
        Session old = active;
        if (ended || old == null || old.dead) return false;
        if (old.front() == front) return true;
        try {
            if (activity.checkSelfPermission(android.Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED
                    || find(front) == null) return false;
        } catch (CameraAccessException e) { return false; }
        active = null;
        release(old);
        Session next = open(front);
        boolean changed = next != null;
        // Most phones cannot open both cameras at once. Restore the previous device on failure.
        active = changed ? next : open(old.front());
        ended = active == null;
        return changed;
    }

    private void release(Session s) {
        if (s == null) return;
        s.dead = true;
        s.torchOn = false;
        CountDownLatch stopped = new CountDownLatch(1);
        handler.post(() -> {
            if (s.capture != null) {
                try {
                    s.request.set(CaptureRequest.FLASH_MODE, CaptureRequest.FLASH_MODE_OFF);
                    s.capture.capture(s.request.build(), null, handler);
                    s.capture.stopRepeating();
                } catch (CameraAccessException | IllegalStateException e) {
                    android.util.Log.w("RustTalk", "Closing unavailable camera", e);
                }
                s.capture.close();
            }
            if (s.device != null) s.device.close();
            if (s.reader != null) s.reader.close();
            stopped.countDown();
        });
        try {
            stopped.await();
            if (s.opening) s.closed.await();
        } catch (InterruptedException e) { Thread.currentThread().interrupt(); }
    }

    void close() {
        ended = true;
        Session s = active;
        active = null;
        release(s);
        if (thread != null) {
            thread.quitSafely();
            try { thread.join(); } catch (InterruptedException e) { Thread.currentThread().interrupt(); }
        }
    }

    private void receive(Session s) {
        try (Image image = s.reader.acquireLatestImage()) {
            if (image == null || ended || s.dead || active != s) return;
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
            frame(s.pixels, rw, rh);
        } catch (IllegalStateException e) {
            if (!s.dead) { ended = true; android.util.Log.w("RustTalk", "Camera frame unavailable", e); }
        }
    }
}
