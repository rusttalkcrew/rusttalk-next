package io.github.rusttalkcrew.rusttalk;

import com.google.androidgamesdk.GameActivity;

/**
 * GameActivity, counting presses of the keyboard's action key (Done). winit drops
 * them, so the app reads the count and turns each new press into Enter
 * (crates/client/src/android.rs).
 */
public class MainActivity extends GameActivity {
    public static volatile int editorActions;

    @Override
    public void onEditorAction(int action) {
        editorActions++;
        super.onEditorAction(action);
    }
}
