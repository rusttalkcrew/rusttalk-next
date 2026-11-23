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
    }
    // R8 strips GameTextInput logging in both release and test packages.
    buildTypes.getByName("release") {
        isMinifyEnabled = true
        proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        if (providers.gradleProperty("rusttalkTestSigning").isPresent) {
            signingConfig = signingConfigs.getByName("debug")
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
