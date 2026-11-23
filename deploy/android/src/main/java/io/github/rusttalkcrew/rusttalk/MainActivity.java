package io.github.rusttalkcrew.rusttalk;

import android.Manifest;
import android.content.Context;
import android.content.pm.PackageManager;
import android.media.AudioManager;
import android.os.Bundle;
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
    private volatile boolean voiceReady;
    private volatile int permission;
    private boolean communicationMode;
    // A join finishing after onDestroy must not leave other apps in call routing.
    private boolean destroyed;

    private static native boolean initializeVoice(Context context);

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
        if (code == MICROPHONE)
            permission = grants.length > 0 && grants[0] == PackageManager.PERMISSION_GRANTED ? 1 : -1;
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
