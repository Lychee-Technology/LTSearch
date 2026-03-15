use std::path::PathBuf;

pub fn protoc() -> PathBuf {
    protoc_bin_vendored::protoc_bin_path().expect("vendored protoc binary should be available")
}

pub fn include() -> PathBuf {
    protoc_bin_vendored::include_path().expect("vendored protobuf includes should be available")
}
