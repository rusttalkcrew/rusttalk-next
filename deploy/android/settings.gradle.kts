// The Android shell around the Rust client: it only packages librusttalk_client.so
// with GameActivity. Built by deploy/android/build.sh (see docs/mobile.md).
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}
rootProject.name = "rusttalk"
