# GameTextInput (in games-activity) logs the text being typed, passwords and
# recovery words too, at debug level. Logcat must never see it.
-assumenosideeffects class android.util.Log {
    public static int d(...);
    public static int v(...);
}
# GameActivity, GameTextInput and MainActivity are reached from native code by name.
-keep class com.google.androidgamesdk.** { *; }
-keep class io.github.rusttalkcrew.rusttalk.** { *; }
# Only the logging goes; names stay readable in crash reports.
-dontobfuscate
# WebRTC looks these up from C++ through JNI.
-keep class livekit.org.webrtc.** { *; }
-keep class livekit.org.jni_zero.** { *; }
# Generated JNI glue the jar doesn't ship; only an unused class-loader setter names it.
-dontwarn livekit.org.jni_zero.JniZeroJni
