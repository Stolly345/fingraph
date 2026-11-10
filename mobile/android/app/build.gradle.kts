import java.util.Properties

plugins {
    id("com.android.application")
    // The Flutter Gradle Plugin must be applied after the Android and Kotlin Gradle plugins.
    id("dev.flutter.flutter-gradle-plugin")
}

// The release key lives outside the public repo: ~/.config/fingraph-android/key.properties,
// or the file named by FINGRAPH_ANDROID_KEYS. Without it, release builds fall back to the debug key.
val keyFile = file(System.getenv("FINGRAPH_ANDROID_KEYS") ?: "${System.getProperty("user.home")}/.config/fingraph-android/key.properties")
val keys = Properties().apply { if (keyFile.exists()) keyFile.inputStream().use { load(it) } }

android {
    namespace = "io.github.stolly345.fingraph"
    compileSdk = flutter.compileSdkVersion
    ndkVersion = "30.0.16248370" // the NDK already installed; Flutter's default is r28

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "io.github.stolly345.fingraph"
        // You can update the following values to match your application needs.
        // For more information, see: https://flutter.dev/to/review-gradle-config.
        // the secure storage and fingerprint plugins need Android 7
        minSdk = maxOf(flutter.minSdkVersion, 24)
        targetSdk = flutter.targetSdkVersion
        // Uses the version code from pubspec.yaml. When using split APKs, 1000 * ABI_VERSION
        // is added automatically by Flutter. (https://developer.android.com/studio/build/configure-apk-splits#configure-APK-versions)
        // You can force using the value of versionCode by specifying the `-P force-version-code-ignoring-abi=true`
        // flag during build.
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        if (keyFile.exists()) {
            create("release") {
                storeFile = file(keys.getProperty("storeFile"))
                storePassword = keys.getProperty("storePassword")
                keyAlias = keys.getProperty("keyAlias")
                keyPassword = keys.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            signingConfig = if (keyFile.exists()) signingConfigs.getByName("release") else {
                logger.warn("FinGraph: no release key found at $keyFile; signing with the debug key")
                signingConfigs.getByName("debug")
            }
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}
