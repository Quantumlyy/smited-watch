fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = "proto";
    let smited_proto = "proto/smited/v1/smited.proto";

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[smited_proto], &[proto_root])?;

    println!("cargo:rerun-if-changed={smited_proto}");
    println!("cargo:rerun-if-changed=proto/buf/validate/validate.proto");
    println!("cargo:rerun-if-changed=build.rs");

    Ok(())
}
