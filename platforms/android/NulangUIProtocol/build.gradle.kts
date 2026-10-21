plugins {
    kotlin("jvm") version "1.9.24"
}

repositories {
    mavenCentral()
}

dependencies {
    // Streaming parser only; no Jackson databind, reflection, or polymorphic DTOs.
    implementation("com.fasterxml.jackson.core:jackson-core:2.22.3")
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

tasks.register<JavaExec>("jsonCodecConformance") {
    group = "verification"
    description = "Execute the strict Kotlin/JVM JSON ingress regression suite"
    dependsOn(tasks.named("testClasses"))
    classpath = sourceSets["test"].runtimeClasspath
    mainClass.set("org.nulang.ui.JsonCodecConformanceTestsKt")
}

tasks.register<JavaExec>("goldenWireConformance") {
    group = "verification"
    description = "Assert shared Rust/Kotlin v1 JSON fixtures and error behavior"
    dependsOn(tasks.named("testClasses"))
    classpath = sourceSets["test"].runtimeClasspath
    mainClass.set("org.nulang.ui.GoldenWireParityTestsKt")
    // Shared fixtures live at repository root, three levels above projectDir.
    workingDir = rootProject.projectDir
}

tasks.named("check") {
    dependsOn("protocolConformance", "jsonCodecConformance", "goldenWireConformance")
}
