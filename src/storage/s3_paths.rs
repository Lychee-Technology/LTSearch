pub const INDEX_HEAD_KEY: &str = "index/_head";

pub fn version_manifest_key(version_id: u64) -> String {
    format!("index/versions/{version_id}/manifest.json")
}
