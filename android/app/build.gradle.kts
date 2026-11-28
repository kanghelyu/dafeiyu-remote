import java.io.ByteArrayOutputStream

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "remote.dafeiyu"
    compileSdk = 36
    buildToolsVersion = "36.0.0"

    defaultConfig {
        applicationId = "remote.dafeiyu"
        minSdk = 29
        targetSdk = 36
        versionCode = 5
        versionName = "0.2.2"
        ndkVersion = "27.1.12297006"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { buildConfig = true }
    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
        }
    }
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity:1.9.3")
    testImplementation("junit:junit:4.13.2")
}

val sdkDir = providers.gradleProperty("android.sdk.path").orNull
    ?: rootProject.file("local.properties").takeIf { it.exists() }?.readLines()
        ?.firstOrNull { it.startsWith("sdk.dir=") }?.substringAfter("=")
    ?: error("Set android.sdk.path or sdk.dir in android/local.properties")

val cargo = providers.environmentVariable("CARGO").orElse("cargo").get()
val androidAbis = (project.findProperty("androidAbis") as String?)?.split(",")
    ?: listOf("arm64-v8a", "x86_64")

tasks.register("cargoBuild") {
    val ndkHome = file("$sdkDir/ndk/27.1.12297006")
    doLast {
        exec {
            workingDir(rootProject.file(".."))
            commandLine(
                listOf(
                    cargo, "ndk", "-t", androidAbis.joinToString(","),
                    "-o", rootProject.file("app/src/main/jniLibs").absolutePath,
                    "build", "--release", "--locked", "-p", "dfy-android"
                )
            )
            environment("ANDROID_NDK_HOME", ndkHome.absolutePath)
        }
    }
}

tasks.register("verifyControllerJniSymbols") {
    dependsOn("cargoBuild")
    doLast {
        val hostTag = when {
            System.getProperty("os.name").startsWith("Windows") -> "windows-x86_64"
            System.getProperty("os.name").startsWith("Mac") -> "darwin-x86_64"
            else -> "linux-x86_64"
        }
        val readObjName = if (hostTag.startsWith("windows")) "llvm-readobj.exe" else "llvm-readobj"
        val readObj = file("$sdkDir/ndk/27.1.12297006/toolchains/llvm/prebuilt/$hostTag/bin/$readObjName")
        val requiredSymbols = listOf("Java_remote_dafeiyu_DfyControllerNative_setQualityControl")
        androidAbis.forEach { abi ->
            val library = rootProject.file("app/src/main/jniLibs/$abi/libdfy_android.so")
            val output = ByteArrayOutputStream()
            exec {
                commandLine(readObj.absolutePath, "--symbols", library.absolutePath)
                standardOutput = output
            }
            val symbols = output.toString(Charsets.UTF_8)
                .lineSequence()
                .flatMap { it.split(Regex("\\s+")).asSequence() }
                .toSet()
            val missing = requiredSymbols.toSet() - symbols
            check(missing.isEmpty()) { "$abi is missing controller JNI symbols: $missing" }
        }
    }
}

tasks.matching { it.name.startsWith("merge") && it.name.endsWith("JniLibFolders") }
    .configureEach { dependsOn("cargoBuild", "verifyControllerJniSymbols") }
