plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.bifrost.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "dev.bifrost.app"
        // 24 (Android 7.0) matches what the Rust core is compiled against.
        minSdk = 24
        targetSdk = 34
        versionCode = 1
        versionName = "1.0.0"

        // Only the ABIs the Rust core is actually built for. Shipping a
        // manifest that claims an ABI with no library in it makes the app
        // install and then die on first launch with UnsatisfiedLinkError.
        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a")
        }
    }

    buildTypes {
        release {
            // No shrinking: R8 strips the JNI entry points unless every one is
            // kept by name, and a silent UnsatisfiedLinkError at runtime is a
            // poor trade for a smaller APK of an app this size.
            isMinifyEnabled = false

            // Signed with the debug key so `gradlew assembleRelease` produces
            // something installable without a keystore. Replace this before
            // ever publishing.
            signingConfig = signingConfigs.getByName("debug")
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

    lint {
        // Lint findings should not gate producing an installable APK; run it
        // deliberately with `gradle lint` instead.
        checkReleaseBuilds = false
        abortOnError = false
    }

    composeOptions {
        // Must match the Kotlin version in the root build file.
        kotlinCompilerExtensionVersion = "1.5.14"
    }

    packaging {
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
        jniLibs {
            // The .so is already stripped by the Rust release profile, and
            // leaving it uncompressed lets Android map it straight from the
            // APK instead of unpacking it on install.
            useLegacyPackaging = false
        }
    }

    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.3")

    val composeBom = platform("androidx.compose:compose-bom:2024.06.00")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")

    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
}
