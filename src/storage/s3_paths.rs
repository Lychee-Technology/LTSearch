pub const INDEX_HEAD_KEY: &str = "index/_head";

pub fn version_manifest_key(version_id: u64) -> String {
    format!("index/versions/{version_id}/manifest.json")
}

pub const STATIC_HEAD_KEY: &str = "static/_head";

pub fn static_release_dir_key(release_id: &str) -> String {
    format!("static/releases/{release_id}")
}

pub fn static_release_manifest_key(release_id: &str) -> String {
    format!("static/releases/{release_id}/release_manifest.json")
}
