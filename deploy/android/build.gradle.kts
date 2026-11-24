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
        val version = providers.gradleProperty("rusttalkVersion").get()
        val (major, minor, patch) = version.split('.').map(String::toInt)
        require(major in 0..2099 && minor in 0..999 && patch in 0..999)
        versionCode = major * 1_000_000 + minor * 1_000 + patch + 1
        versionName = version
        manifestPlaceholders["appLabel"] = "RustTalk"
    }
    // R8 strips GameTextInput logging in both release and test packages.
    buildTypes.getByName("release") {
        isMinifyEnabled = true
        proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        if (providers.gradleProperty("rusttalkTestSigning").isPresent) {
            signingConfig = signingConfigs.getByName("debug")
            // Each runner has its own debug key. A test install under the release
            // package name blocks the release APK ("App not installed", #508), even
            // when it only remains in another profile, so test builds get their own.
            applicationIdSuffix = ".test"
            versionNameSuffix = "-test"
            manifestPlaceholders["appLabel"] = "RustTalk Test"
        }
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
    implementation(files("build/rust/libwebrtc.jar"))
    // The version android-activity 0.6 is written against (see docs/mobile.md).
    implementation("androidx.games:games-activity:4.4.0")
    // GameActivity is an AppCompatActivity and needs an AppCompat theme.
    implementation("androidx.appcompat:appcompat:1.7.1")
}
