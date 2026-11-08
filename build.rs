// On macOS, builds src/secure_enclave.swift into a static library for src/sealed.rs, with the Swift
// compiler from the Xcode command line tools (the same toolchain that already links pai on a Mac).
// The Swift runtime and CryptoKit ship with macOS, so the binary adds no file of its own.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/secure_enclave.swift");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        other => panic!("pai: no Secure Enclave build for {other:?}"),
    };
    let lib = out.join("libpai_secure_enclave.a");
    let status = Command::new("xcrun")
        .args([
            "swiftc",
            "-emit-library",
            "-static",
            "-parse-as-library",
            "-O",
        ])
        .args(["-module-name", "PaiSecureEnclave", "-target"])
        .arg(format!("{arch}-apple-macos11"))
        .arg("-o")
        .arg(&lib)
        .arg("src/secure_enclave.swift")
        .status()
        .expect("pai: run xcrun swiftc (install the Xcode command line tools)");
    assert!(
        status.success(),
        "pai: swiftc failed on src/secure_enclave.swift"
    );
    let sdk = String::from_utf8(
        Command::new("xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .expect("xcrun")
            .stdout,
    )
    .unwrap();
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=pai_secure_enclave");
    // The Swift runtime: link against the SDK's stubs, load the copy built into macOS.
    println!(
        "cargo:rustc-link-search=native={}/usr/lib/swift",
        sdk.trim()
    );
    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    // The static back-deployment shims (swiftCompatibility56, swiftCompatibilityConcurrency, ...)
    // that swiftc auto-links for a macOS 11 target ship only in the toolchain, next to swiftc.
    let swiftc = String::from_utf8(
        Command::new("xcrun")
            .args(["--find", "swiftc"])
            .output()
            .expect("xcrun --find swiftc")
            .stdout,
    )
    .unwrap();
    let toolchain = PathBuf::from(swiftc.trim());
    let shims = toolchain
        .parent()
        .and_then(|bin| bin.parent())
        .expect("pai: swiftc path")
        .join("lib/swift/macosx");
    println!("cargo:rustc-link-search=native={}", shims.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for lib in ["swiftCore", "swiftFoundation"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
    for framework in ["CryptoKit", "Foundation", "Security"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
