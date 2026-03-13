fn main() {
    #[cfg(feature = "generate-proto")]
    {
        if std::process::Command::new("protoc")
            .arg("--version")
            .output()
            .is_ok()
        {
            println!("Using existing protoc installation");
        } else {
            dlprotoc::download_protoc().unwrap();
        }

        tonic_build::configure()
            .out_dir("src/generated/")
            .build_client(true)
            .file_descriptor_set_path("src/generated/provider_service_descriptor.bin")
            .compile_protos(&["../../proto/provider.proto"], &["../../proto/"])
            .unwrap();
    }

    #[cfg(not(feature = "generate-proto"))]
    {
        use std::path::Path;
        let provider_rs = Path::new("src/generated/provider.rs");
        let descriptor_bin = Path::new("src/generated/provider_service_descriptor.bin");

        if !provider_rs.exists() {
            panic!(
                "Pre-generated proto file not found: {}. Run with --features generate-proto to regenerate.",
                provider_rs.display()
            );
        }
        if !descriptor_bin.exists() {
            panic!(
                "Pre-generated descriptor file not found: {}. Run with --features generate-proto to regenerate.",
                descriptor_bin.display()
            );
        }
    }
}
