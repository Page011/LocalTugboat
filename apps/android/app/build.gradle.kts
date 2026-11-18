plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "app.localtugboat"
    compileSdk = 37

    defaultConfig {
        applicationId = providers.gradleProperty("tugboat.applicationId").get()
        minSdk = 31
        targetSdk = 37
        versionCode = 4
        versionName = "0.2.0"
        // Only the two ABIs build-rust.cmd produces: arm64-v8a (phone) and x86_64 (emulator).
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    lint {
        // "A newer Gradle / library exists" notices, not defects: the AGP, Gradle and JNA versions are pinned on purpose
        // (JNA carries libjnidispatch for the UniFFI bindings). Every other check stays on.
        disable += listOf("AndroidGradlePluginVersion", "NewerVersionAvailable")
    }

    buildFeatures {
        compose = true
        // BuildConfig.DEBUG gates the paste-pairing-code field and other test-only paths.
        buildConfig = true
    }

    buildTypes {
        release {
            // R8 off for now. No signingConfig: the AAB is built UNSIGNED, the user signs it with jarsigner.
            isMinifyEnabled = false
        }
    }

}

dependencies {
    // UniFFI bindings load libtugboat_ffi.so through JNA (the aar carries libjnidispatch.so).
    implementation("net.java.dev.jna:jna:5.17.0@aar")

    implementation("androidx.core:core:1.19.1")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.11.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.11.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.11.0")

    val bom = platform("androidx.compose:compose-bom:2026.09.00")
    implementation(bom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")

    val camerax = "1.6.2"
    implementation("androidx.camera:camera-core:$camerax")
    implementation("androidx.camera:camera-camera2:$camerax")
    implementation("androidx.camera:camera-lifecycle:$camerax")
    implementation("androidx.camera:camera-view:$camerax")
    implementation("androidx.camera:camera-video:$camerax")

    // Bundled model: no Google Play Services needed to scan the pairing QR.
    implementation("com.google.mlkit:barcode-scanning:17.3.0")

    val media3 = "1.11.1"
    implementation("androidx.media3:media3-exoplayer:$media3")
    implementation("androidx.media3:media3-ui:$media3")

    implementation("androidx.work:work-runtime:2.12.0")

    // Photo orientation (lint: the platform android.media.ExifInterface is the old copy).
    implementation("androidx.exifinterface:exifinterface:1.4.2")

    testImplementation("junit:junit:4.13.2")
}
