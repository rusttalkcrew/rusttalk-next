# GameTextInput (in games-activity) logs the text being typed, passwords and
# recovery words too, at debug level. Logcat must never see it.
-assumenosideeffects class android.util.Log {
    public static int d(...);
    public static int v(...);
}
# GameActivity and GameTextInput are reached from native code by name.
-keep class com.google.androidgamesdk.** { *; }
# Only the logging goes; names stay readable in crash reports.
-dontobfuscate
