package io.github.rusttalkcrew.rusttalk;

import android.content.Context;
import java.nio.ByteBuffer;
import livekit.org.webrtc.Camera2Enumerator;
import livekit.org.webrtc.CameraVideoCapturer;
import livekit.org.webrtc.CapturerObserver;
import livekit.org.webrtc.SurfaceTextureHelper;
import livekit.org.webrtc.VideoFrame;
import livekit.org.webrtc.YuvHelper;

final class CallCamera implements CapturerObserver {
    private CameraVideoCapturer camera;
    private SurfaceTextureHelper textures;
    private ByteBuffer pixels;
    volatile boolean ended;

    private static native void frame(ByteBuffer pixels, int width, int height);

    boolean start(Context context, boolean front, int width, int height, int fps) {
        Camera2Enumerator devices = new Camera2Enumerator(context);
        for (String id : devices.getDeviceNames()) {
            if (!(front ? devices.isFrontFacing(id) : devices.isBackFacing(id))) continue;
            camera = devices.createCapturer(id, null);
            if (camera == null) continue;
            textures = SurfaceTextureHelper.create("RustTalk camera", null);
            if (textures == null) return false;
            camera.initialize(textures, context, this);
            camera.startCapture(width, height, fps);
            return true;
        }
        return false;
    }

    void close() {
        ended = true;
        if (camera != null) {
            camera.dispose(); // Includes stopCapture; waits for the camera thread.
        }
        if (textures != null) textures.dispose();
    }

    @Override public void onCapturerStarted(boolean success) { if (!success) ended = true; }
    @Override public void onCapturerStopped() { ended = true; }

    @Override public void onFrameCaptured(VideoFrame input) {
        if (ended) return;
        VideoFrame.I420Buffer b = input.getBuffer().toI420();
        if (b == null) return;
        try {
            int w = input.getRotatedWidth(), h = input.getRotatedHeight();
            int size = w * h + 2 * ((w + 1) / 2) * ((h + 1) / 2);
            if (pixels == null || pixels.capacity() != size) pixels = ByteBuffer.allocateDirect(size);
            YuvHelper.I420Rotate(b.getDataY(), b.getStrideY(), b.getDataU(), b.getStrideU(),
                    b.getDataV(), b.getStrideV(), pixels, b.getWidth(), b.getHeight(), input.getRotation());
            frame(pixels, w, h);
        } finally {
            b.release();
        }
    }
}
