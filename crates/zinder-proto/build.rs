//! Build script for Zinder-owned protobuf modules.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let descriptor_set_path =
        std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("zinder_v1_descriptor.bin");
    let compat_proto_dir = "proto/compat/lightwalletd";
    let compat_commit_file = format!("{compat_proto_dir}/COMMIT");
    let native_proto_dir = "proto";
    let native_wallet_proto_dir = "proto/zinder/v1/wallet";
    let native_ingest_proto_dir = "proto/zinder/v1/ingest";
    let compat_proto_files = [
        format!("{compat_proto_dir}/compact_formats.proto"),
        format!("{compat_proto_dir}/service.proto"),
    ];
    let native_proto_files = [
        format!("{native_wallet_proto_dir}/wallet.proto"),
        format!("{native_ingest_proto_dir}/ingest.proto"),
    ];
    let compat_include_dirs = [compat_proto_dir.to_owned()];
    let include_dirs = [native_proto_dir.to_owned()];

    for proto_file in compat_proto_files.iter().chain(native_proto_files.iter()) {
        println!("cargo:rerun-if-changed={proto_file}");
    }
    println!("cargo:rerun-if-changed={compat_commit_file}");
    println!(
        "cargo:rustc-env=LIGHTWALLETD_PROTOCOL_COMMIT={}",
        std::fs::read_to_string(&compat_commit_file)?.trim()
    );

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&compat_proto_files, &compat_include_dirs)?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(&descriptor_set_path)
        .compile_protos(&native_proto_files, &include_dirs)?;

    Ok(())
}
