use std::env;

fn main() {
    // Capture build-time environment variables
    let cflags = env::var("CFLAGS").unwrap_or_else(|_| String::from("(not set)"));
    let cxxflags = env::var("CXXFLAGS").unwrap_or_else(|_| String::from("(not set)"));
    let ldflags = env::var("LDFLAGS").unwrap_or_else(|_| String::from("(not set)"));
    let rustflags = env::var("RUSTFLAGS").unwrap_or_else(|_| String::from("(not set)"));
    let cargo_profile_release_lto = env::var("CARGO_PROFILE_RELEASE_LTO")
        .unwrap_or_else(|_| String::from("(not set)"));
    let cargo_profile_release_codegen_units = env::var("CARGO_PROFILE_RELEASE_CODEGEN_UNITS")
        .unwrap_or_else(|_| String::from("(not set)"));

    // Export these as compile-time environment variables
    println!("cargo:rustc-env=BUILD_CFLAGS={}", cflags);
    println!("cargo:rustc-env=BUILD_CXXFLAGS={}", cxxflags);
    println!("cargo:rustc-env=BUILD_LDFLAGS={}", ldflags);
    println!("cargo:rustc-env=BUILD_RUSTFLAGS={}", rustflags);
    println!("cargo:rustc-env=BUILD_CARGO_PROFILE_RELEASE_LTO={}", cargo_profile_release_lto);
    println!("cargo:rustc-env=BUILD_CARGO_PROFILE_RELEASE_CODEGEN_UNITS={}", cargo_profile_release_codegen_units);

    // Re-run if environment changes
    println!("cargo:rerun-if-env-changed=CFLAGS");
    println!("cargo:rerun-if-env-changed=CXXFLAGS");
    println!("cargo:rerun-if-env-changed=LDFLAGS");
    println!("cargo:rerun-if-env-changed=RUSTFLAGS");
    println!("cargo:rerun-if-env-changed=CARGO_PROFILE_RELEASE_LTO");
    println!("cargo:rerun-if-env-changed=CARGO_PROFILE_RELEASE_CODEGEN_UNITS");
}
