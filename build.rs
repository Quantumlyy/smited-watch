fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = "proto";
    let smited_proto = "proto/smited/v1/smited.proto";

    // tonic-prost-build delegates to prost-build, which shells out to a
    // `protoc` binary. Without this, a fresh `cargo install` would fail
    // unless the user had installed protobuf-compilers via their system
    // package manager — contradicting the README's "only a Rust toolchain"
    // quickstart. `protoc-bin-vendored` ships prebuilt binaries for major
    // host triples; we point prost-build at that copy via the `PROTOC`
    // env var (only when not already set, so power users with their own
    // `PROTOC` keep control).
    if std::env::var_os("PROTOC").is_none() {
        let protoc = protoc_bin_vendored::protoc_bin_path()?;
        std::env::set_var("PROTOC", protoc);
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[smited_proto], &[proto_root])?;

    println!("cargo:rerun-if-changed={smited_proto}");
    println!("cargo:rerun-if-changed=proto/buf/validate/validate.proto");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PROTOC");

    Ok(())
}
