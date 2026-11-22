package io.github.rusttalkcrew.rusttalk;

import android.view.inputmethod.EditorInfo;
import com.google.androidgamesdk.GameActivity;

/**
 * GameActivity, counting presses of the keyboard's Done key. winit drops editor
 * actions, so the app reads the count and turns each new press into Enter
 * (crates/client/src/android.rs). Other actions (Next, Search) are not Enter.
 */
public class MainActivity extends GameActivity {
    public static volatile int editorActions;

    @Override
    public void onEditorAction(int action) {
        if (action == EditorInfo.IME_ACTION_DONE) {
            editorActions++;
        }
        super.onEditorAction(action);
    }
}
