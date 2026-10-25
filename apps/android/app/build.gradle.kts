import javax.inject.Inject
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

/**
 * The repository's page packages (../../pages/<name>/: page.json, .luau,
 * .wgsl and binary samples under assets/) go into the APK as
 * assets/pages/<name>/. Generated at build time so the APK always carries
 * the packages as they are in the repository; nothing is copied into
 * src/main/assets by hand.
 */
abstract class SyncPages : DefaultTask() {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val pagesDir: DirectoryProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val fs: FileSystemOperations

    @TaskAction
    fun sync() {
        fs.sync {
            from(pagesDir) {
                into("pages")
                // editor and OS droppings never ship
                exclude("**/.*", "**/*~", "**/*.bak", "**/*.tmp", "**/*.orig", "**/Thumbs.db", "**/desktop.ini")
            }
            into(outputDir)
        }
    }
}

val syncPages = tasks.register<SyncPages>("syncPages") {
    pagesDir.set(rootProject.layout.projectDirectory.dir("../../pages"))
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

androidComponents {
    onVariants { variant ->
        variant.sources.assets?.addGeneratedSourceDirectory(syncPages, SyncPages::outputDir)
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}
