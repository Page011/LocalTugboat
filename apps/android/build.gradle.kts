// AGP 9.5.1 needs Gradle 9.6.0 and JDK 17+. Kotlin is built into AGP 9 (no kotlin-android plugin).
// The Compose compiler plugin must match the Kotlin version AGP brings (2.2.10).
plugins {
    id("com.android.application") version "9.5.1" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.10" apply false
}
