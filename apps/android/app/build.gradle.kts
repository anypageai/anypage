import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.anypage.engine"
    compileSdk = 35
    // lets Gradle strip debug info from the engine library when packaging
    ndkVersion = "27.1.12297006"

    defaultConfig {
        applicationId = "com.anypage.engine"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.0.1"
        ndk {
            // the engine library is built by cargo-ndk into src/main/jniLibs
            abiFilters += listOf("arm64-v8a", "x86_64")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}
