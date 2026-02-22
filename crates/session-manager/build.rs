fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("../../proto/agent.proto")?;

    // CI sets APP_VERSION from git tag; fall back to Cargo.toml version
    if let Ok(v) = std::env::var("APP_VERSION") {
        println!("cargo:rustc-env=APP_VERSION={v}");
    } else {
        println!("cargo:rustc-env=APP_VERSION={}", env!("CARGO_PKG_VERSION"));
    }
    println!("cargo:rerun-if-env-changed=APP_VERSION");

    Ok(())
}
