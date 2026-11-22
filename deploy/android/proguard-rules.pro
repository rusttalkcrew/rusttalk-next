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
