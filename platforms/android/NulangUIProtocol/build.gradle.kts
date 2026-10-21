plugins {
    kotlin("jvm") version "1.9.24"
}

repositories {
    mavenCentral()
}

kotlin {
    jvmToolchain(17)
}

// Intentionally no JSON, Compose, Android SDK or JUnit dependency in this
// portability baseline. Conformance is an executable suite, not a zero-test JUnit pass.
tasks.register<JavaExec>("protocolConformance") {
    group = "verification"
    description = "Execute the Kotlin/JVM universal UI protocol conformance suite"
    dependsOn(tasks.named("testClasses"))
    classpath = sourceSets["test"].runtimeClasspath
    mainClass.set("org.nulang.ui.ProtocolConformanceTestsKt")
}

tasks.named("check") {
    dependsOn("protocolConformance")
}
