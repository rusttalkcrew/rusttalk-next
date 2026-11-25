package io.github.rusttalkcrew.rusttalk;

import android.Manifest;
import android.content.Context;
import android.content.pm.PackageManager;
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
import android.view.inputmethod.EditorInfo;
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

    // FLAG_SECURE while the recovery words show: screenshots, screen recordings and the
    // recents thumbnail get a blank window (crates/client/src/app.rs, secure_screen).
    public void secureWindow(boolean on) {
        int flag = android.view.WindowManager.LayoutParams.FLAG_SECURE;
        runOnUiThread(() -> {
            if (on) getWindow().addFlags(flag);
            else getWindow().clearFlags(flag);
        });
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
        communicationMode = on;
    }

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
}
