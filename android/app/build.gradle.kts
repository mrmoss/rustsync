plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// A fixed signing key makes updates install over the previous version. Without one (no
// ANDROID_KEYSTORE_FILE), the app is signed with a throw-away debug key: it installs fine, but
// the next build can only be installed after uninstalling this one (which loses its data).
val stableKeystore: String? = System.getenv("ANDROID_KEYSTORE_FILE")?.takeIf { it.isNotBlank() && file(it).exists() }

android {
    namespace = "app.rustsync"
    compileSdk = 35

    defaultConfig {
        applicationId = "app.rustsync"
        minSdk = 26
        targetSdk = 35
        // CI passes the version from Cargo.toml; a local build uses 0.1.0. The version code (what
        // Android compares to decide "newer") comes from that version, so it only ever goes up
        // when the version does: 0.12.2 becomes 12002, 1.2.3 becomes 1002003. (A CI run counter
        // would not do: the push builds and the release builds count separately.)
        val appVersion = (project.findProperty("appVersion") as String?) ?: "0.1.0"
        val parts = appVersion.split(".", "-", "+").mapNotNull { it.toIntOrNull() }
        versionCode = (parts.getOrElse(0) { 0 } * 1_000_000 + parts.getOrElse(1) { 1 } * 1_000 + parts.getOrElse(2) { 0 }).coerceAtLeast(1)
        versionName = appVersion
    }

    signingConfigs {
        if (stableKeystore != null) {
            create("stable") {
                storeFile = file(stableKeystore)
                storePassword = System.getenv("ANDROID_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("ANDROID_KEY_ALIAS")
                keyPassword = System.getenv("ANDROID_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = if (stableKeystore != null) {
                signingConfigs.getByName("stable")
            } else {
                signingConfigs.getByName("debug")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2024.10.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.core:core-ktx:1.15.0")
    // QR codes: drawing them and scanning them with the camera
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
    implementation("com.google.zxing:core:3.5.3")
}
