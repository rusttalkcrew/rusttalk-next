plugins {
    id("com.android.application") version "9.5.2"
}

android {
    namespace = "io.github.rusttalkcrew.rusttalk"
    compileSdk = 36
    defaultConfig {
        applicationId = "io.github.rusttalkcrew.rusttalk"
        // AAudio, which cpal plays and records through, came in Android 8.0.
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = providers.gradleProperty("rusttalkVersion").getOrElse("0.0.0")
    }
    // build.sh already stripped the debug info; function names stay for panic backtraces.
    packaging.jniLibs.keepDebugSymbols += "**/*.so"
    // Filled by build.sh: the Rust library, the app icon, and LICENSE and NOTICE,
    // which Apache-2.0 requires in every copy.
    sourceSets["main"].jniLibs.directories.add("build/rust/jniLibs")
    sourceSets["main"].res.directories.add("build/rust/res")
    sourceSets["main"].assets.directories.add("build/rust/assets")
}

dependencies {
    // The version android-activity 0.6 is written against (see docs/mobile.md).
    implementation("androidx.games:games-activity:4.4.0")
    // GameActivity is an AppCompatActivity and needs an AppCompat theme.
    implementation("androidx.appcompat:appcompat:1.7.1")
}
