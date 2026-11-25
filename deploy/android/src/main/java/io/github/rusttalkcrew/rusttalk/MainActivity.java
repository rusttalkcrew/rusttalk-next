package io.github.rusttalkcrew.rusttalk;

import android.Manifest;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.os.Build;
import android.os.Bundle;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.security.keystore.StrongBoxUnavailableException;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;
import android.util.Log;
import android.view.KeyEvent;
import android.view.inputmethod.EditorInfo;
import androidx.activity.OnBackPressedCallback;
import com.google.androidgamesdk.GameActivity;

/**
 * GameActivity, counting presses of the keyboard's Done key. winit drops editor
 * actions, so the app reads the count and turns each new press into Enter
 * (crates/client/src/android.rs). Other actions (Next, Search) are not Enter.
 */
public class MainActivity extends GameActivity {
    public static volatile int editorActions;

    private static final int MICROPHONE = 1;
    private static final int CAMERA = 2;
    private static final int NOTIFICATIONS = 3;
    private volatile boolean voiceReady;
    private volatile int permission;
    private boolean communicationMode;
    // A join finishing after onDestroy must not leave other apps in call routing.
    private boolean destroyed;

    private static native boolean initializeVoice(Context context);

    private static KeyStore identityStore() throws Exception {
        KeyStore store = KeyStore.getInstance("AndroidKeyStore");
        store.load(null);
        return store;
    }

    private SecretKey createIdentityKey(String alias) throws GeneralSecurityException {
        KeyGenParameterSpec.Builder spec = new KeyGenParameterSpec.Builder(alias,
                KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                .setKeySize(256).setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setUserAuthenticationRequired(false);
        KeyGenerator generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
        if (Build.VERSION.SDK_INT >= 28 && getPackageManager().hasSystemFeature(
                PackageManager.FEATURE_STRONGBOX_KEYSTORE)) {
            try {
                generator.init(spec.setIsStrongBoxBacked(true).build());
                return generator.generateKey();
            } catch (StrongBoxUnavailableException unavailable) {
                spec.setIsStrongBoxBacked(false);
            }
        }
        generator.init(spec.build());
        return generator.generateKey();
    }

    // The provider chooses a fresh 12-byte IV for every encryption. The remaining
    // bytes are ciphertext followed by the 128-bit GCM authentication tag.
    public byte[] cryptIdentity(String alias, boolean encrypt, byte[] input) throws Exception {
        try {
            SecretKey key = (SecretKey) identityStore().getKey(alias, null);
            if (key == null) {
                if (!encrypt) throw new GeneralSecurityException("Missing Android identity key");
                key = createIdentityKey(alias);
            }
            Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
            if (!encrypt) {
                if (input.length < 28) throw new GeneralSecurityException("Truncated wrapped identity");
                cipher.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, input, 0, 12));
                return cipher.doFinal(input, 12, input.length - 12);
            }
            cipher.init(Cipher.ENCRYPT_MODE, key);
            byte[] iv = cipher.getIV();
            if (iv.length != 12) throw new GeneralSecurityException("Unexpected Keystore IV length");
            byte[] blob = Arrays.copyOf(iv, iv.length + cipher.getOutputSize(input.length));
            cipher.doFinal(input, 0, input.length, blob, iv.length);
            return blob;
        } finally {
            if (encrypt) Arrays.fill(input, (byte) 0);
        }
    }

    public void probeIdentityKey(String identityAlias) throws Exception {
        String alias = identityAlias + "/probe";
        deleteIdentityKey(alias);
        try {
            byte[] blob = cryptIdentity(alias, true, new byte[]{42});
            if (!Arrays.equals(cryptIdentity(alias, false, blob), new byte[]{42}))
                throw new GeneralSecurityException("Android Keystore probe failed");
        } finally {
            deleteIdentityKey(alias);
        }
    }

    public void deleteIdentityKey(String alias) throws Exception {
        identityStore().deleteEntry(alias);
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        voiceReady = initializeVoice(getApplicationContext());
        if (!voiceReady) Log.e("RustTalk", "WebRTC initialization failed; voice disabled");
        getOnBackPressedDispatcher().addCallback(this, back);
    }

    // 0 while the system prompt is open, 1 to join, -1 for denial, -2 for init failure.
    public int voicePermission(boolean request) {
        if (!voiceReady) return -2;
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED)
            return 1;
        if (request) {
            permission = 0;
            runOnUiThread(() -> requestPermissions(new String[]{Manifest.permission.RECORD_AUDIO}, MICROPHONE));
        }
        return permission;
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] names, int[] grants) {
        super.onRequestPermissionsResult(code, names, grants);
        if (code == CAMERA)
            cameraGrant = grants.length > 0 && grants[0] == PackageManager.PERMISSION_GRANTED ? 1 : -1;
        if (code == MICROPHONE)
            permission = grants.length > 0 && grants[0] == PackageManager.PERMISSION_GRANTED ? 1 : -1;
    }

    private volatile int cameraGrant;
    private boolean resumed;
    private CallCamera camera;

    public synchronized int cameraPermission(boolean request) {
        if (checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
            return resumed ? 1 : 0;
        if (request) {
            cameraGrant = 0;
            runOnUiThread(() -> requestPermissions(new String[]{Manifest.permission.CAMERA}, CAMERA));
        }
        return cameraGrant == 0 ? 0 : -1;
    }

    public int cameraDevices() {
        livekit.org.webrtc.Camera2Enumerator devices = new livekit.org.webrtc.Camera2Enumerator(this);
        int mask = 0;
        for (String id : devices.getDeviceNames()) {
            if (devices.isFrontFacing(id)) mask |= 1;
            if (devices.isBackFacing(id)) mask |= 2;
        }
        return mask;
    }

    /**
     * Keep a strip along the left edge for the app's own swipe (the channel list
     * drawer on phones) instead of the system Back gesture; an empty rect gives it
     * back. Window pixels. Android honours at most 200 dp of exclusions per edge
     * and drops the rest, so the band is clamped to a centred 200 dp here too (the
     * app already asks for no more); the rest of the edge stays Back.
     */
    public void excludeLeftEdge(int left, int top, int right, int bottom) {
        if (Build.VERSION.SDK_INT < 29) return;
        runOnUiThread(() -> {
            android.view.View view = findViewById(android.R.id.content);
            if (view == null) return;
            java.util.List<android.graphics.Rect> rects = new java.util.ArrayList<>();
            if (right > left && bottom > top) {
                int cap = (int) (200 * getResources().getDisplayMetrics().density);
                int middle = (top + bottom) / 2;
                int half = Math.min(bottom - top, cap) / 2;
                int[] at = new int[2];
                view.getLocationInWindow(at);
                rects.add(new android.graphics.Rect(left - at[0], middle - half - at[1],
                        right - at[0], middle + half - at[1]));
            }
            view.setSystemGestureExclusionRects(rects);
        });
    }

    public boolean startCamera(boolean front, int w, int h, int fps, long generation) {
        CallCamera next = new CallCamera(this, w, h, fps, generation);
        synchronized (this) {
            if (!resumed || !voiceReady || camera != null) return false;
            camera = next;
        }
        return next.start(front);
    }

    public synchronized int cameraState() { return camera == null ? 0 : camera.state(); }
    public boolean selectCamera(boolean front) {
        CallCamera current;
        synchronized (this) { current = resumed ? camera : null; }
        return current != null && current.select(front);
    }
    public boolean cameraTorch(boolean on) {
        CallCamera current;
        synchronized (this) { current = resumed ? camera : null; }
        return current != null && current.torch(on);
    }

    public synchronized boolean cameraEnded() { return camera == null || camera.ended(); }

    public synchronized void stopCamera() {
        // cancel only invalidates and posts cleanup; it never waits for native work.
        if (camera != null) camera.cancel();
        camera = null;
    }

    @Override protected void onResume() {
        super.onResume();
        synchronized (this) { resumed = true; }
    }

    @Override protected void onPause() {
        synchronized (this) { resumed = false; stopCamera(); }
        super.onPause();
    }

    public synchronized void communicationMode(boolean on) {
        if (communicationMode == on || (on && destroyed)) return;
        AudioManager audio = getSystemService(AudioManager.class);
        // MODE_NORMAL releases this app's mode request and restores system routing.
        audio.setMode(on ? AudioManager.MODE_IN_COMMUNICATION : AudioManager.MODE_NORMAL);
        if (on) {
            if (audio.requestAudioFocus(focus) != AudioManager.AUDIOFOCUS_REQUEST_GRANTED)
                Log.w("RustTalk", "Call audio focus denied");
        } else {
            audio.abandonAudioFocusRequest(focus);
        }
        Intent service = new Intent(this, CallService.class);
        if (on) {
            // Without the permission (Android 13+) the service still runs; only its
            // notification is hidden. The system stops asking after two denials.
            if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                    != PackageManager.PERMISSION_GRANTED)
                runOnUiThread(() -> requestPermissions(
                        new String[]{Manifest.permission.POST_NOTIFICATIONS}, NOTIFICATIONS));
            // A plain start: joining happens on screen, and the service then moves
            // itself to the foreground (a startForegroundService that fails there
            // would crash the app).
            try {
                startService(service);
            } catch (IllegalStateException | SecurityException error) {
                Log.w("RustTalk", "Call service not started", error);
            }
        } else {
            stopService(service);
        }
        communicationMode = on;
    }

    private final AudioFocusRequest focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
            .setAudioAttributes(new AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build())
            .build();

    @Override
    protected void onDestroy() {
        synchronized (this) {
            destroyed = true;
            communicationMode(false);
        }
        super.onDestroy();
    }

    @Override
    public void onEditorAction(int action) {
        if (action == EditorInfo.IME_ACTION_DONE) {
            editorActions++;
        }
        super.onEditorAction(action);
    }

    /**
     * Back goes to the app (crates/client/src/back.rs) while it has something to
     * close, which it says through backEnabled. At its root the callback is off and
     * Android handles Back: with predictive back (Android 13+, the manifest's
     * enableOnBackInvokedCallback) it shows the home screen behind the app.
     */
    private static native void backPressed();

    private final OnBackPressedCallback back = new OnBackPressedCallback(false) {
        @Override
        public void handleOnBackPressed() {
            backPressed();
        }
    };

    public void backEnabled(boolean on) {
        runOnUiThread(() -> back.setEnabled(on));
    }

    public void toBackground() {
        runOnUiThread(() -> moveTaskToBack(true));
    }

    // Without predictive back, GameActivity hands the Back key to native code and
    // never reaches onBackPressed. Send it to the app instead, on release like
    // Activity, so the app decides (and moves itself to the background at its root).
    @Override
    public boolean onKeyDown(int code, KeyEvent event) {
        if (code != KeyEvent.KEYCODE_BACK) return super.onKeyDown(code, event);
        event.startTracking();
        return true;
    }

    @Override
    public boolean onKeyUp(int code, KeyEvent event) {
        if (code != KeyEvent.KEYCODE_BACK) return super.onKeyUp(code, event);
        if (event.isTracking() && !event.isCanceled()) backPressed();
        return true;
    }
}
