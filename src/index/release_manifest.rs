//! TurboQuant v3 static release 的自描述 manifest 与内容导出 release_id。
//!
//! 决定性构建的核心：manifest 中不含时间戳 / UUID / HashMap 序列化，
//! 因此 build-twice 逐字节相同。所有 digest / release_id 均为纯函数，可单测。

use crate::models::CorpusType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// release manifest 在磁盘上的固定文件名。
pub const RELEASE_MANIFEST_FILE: &str = "release_manifest.json";

/// TurboQuant v3 static release 的自描述 manifest。
///
/// `outputs` 在写入前须按 `name` 升序排序，以保证序列化字节稳定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub manifest_schema_version: u32,
    pub turbo_version: u32,
    pub release_id: String,
    pub source: ReleaseSource,
    pub embedding_profile: EmbeddingProfile,
    pub input_fingerprint: InputFingerprint,
    pub codec: CodecMetadata,
    pub outputs: Vec<OutputFile>,
}

/// release 的来源信息。**整体排除在 release_id 之外**：同内容不同磁盘路径
/// 应得到相同 release_id。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseSource {
    pub kind: String,
    pub dataset_path: String,
    pub table_version: u64,
    pub table_row_count: u64,
    pub corpus_type: CorpusType,
}

/// embedding 模型标识与维度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingProfile {
    pub model_id: String,
    pub dim: u32,
}

/// 输入内容指纹。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputFingerprint {
    pub doc_count: u64,
    pub content_digest: String,
}

/// TurboQuant codec 的决定性参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodecMetadata {
    pub dim: u32,
    pub centroids_per_dim: u32,
    pub centroids_seed: u64,
    pub projection_seed: u64,
}

/// 单个产出文件的名称、内容 sha256(hex) 与字节大小。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputFile {
    pub name: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// 已按 `doc_id` 排序的规范化行(排序由调用方保证)。
///
/// `canonical_meta_json` 应由 [`canonical_metadata_json`] 产出,以保证字节稳定。
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalRow {
    pub doc_id: String,
    pub embedding: Vec<f32>,
    pub text: String,
    pub canonical_meta_json: Vec<u8>,
}

/// 一次性 sha256 → hex 字符串。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 把 metadata HashMap 重排进有序 BTreeMap 后 `to_vec`,得到与插入顺序无关的
/// 规范字节。这是唯一可信的 metadata 字节来源。
pub fn canonical_metadata_json(metadata: &HashMap<String, Value>) -> Vec<u8> {
    let ordered: BTreeMap<&String, &Value> = metadata.iter().collect();
    serde_json::to_vec(&ordered).expect("BTreeMap<String, Value> serialization cannot fail")
}

/// 对**已按 doc_id 排序**的行序列计算内容 digest(hex)。
///
/// 每行按 `doc_id ∥ embedding ∥ text ∥ canonical_meta_json` 顺序流式喂入,
/// 每个可变长字段前置 8 字节小端长度前缀以消除拼接歧义。
pub fn content_digest(rows: &[CanonicalRow]) -> String {
    let mut hasher = Sha256::new();
    for row in rows {
        update_len_prefixed(&mut hasher, row.doc_id.as_bytes());

        hasher.update((row.embedding.len() as u64).to_le_bytes());
        for value in &row.embedding {
            hasher.update(value.to_le_bytes());
        }

        update_len_prefixed(&mut hasher, row.text.as_bytes());
        update_len_prefixed(&mut hasher, &row.canonical_meta_json);
    }
    hex::encode(hasher.finalize())
}

/// 从**内容分量**导出 release_id(hex)。
///
/// 参与:`turbo_version` ∥ `profile`(model_id 长度前缀 + dim) ∥
/// `codec`(dim/centroids_per_dim/centroids_seed/projection_seed) ∥
/// `content_digest`(hex 字符串字节) ∥ 按 name 升序的 `outputs`
/// (name 长度前缀 + sha256 字节 + size_bytes)。
///
/// **排除整个 `source`**:同内容不同磁盘路径 → 同 release_id。
pub fn derive_release_id(
    turbo_version: u32,
    profile: &EmbeddingProfile,
    codec: &CodecMetadata,
    content_digest: &str,
    outputs: &[OutputFile],
) -> String {
    let mut hasher = Sha256::new();

    hasher.update(turbo_version.to_le_bytes());

    update_len_prefixed(&mut hasher, profile.model_id.as_bytes());
    hasher.update(profile.dim.to_le_bytes());

    hasher.update(codec.dim.to_le_bytes());
    hasher.update(codec.centroids_per_dim.to_le_bytes());
    hasher.update(codec.centroids_seed.to_le_bytes());
    hasher.update(codec.projection_seed.to_le_bytes());

    update_len_prefixed(&mut hasher, content_digest.as_bytes());

    let mut sorted: Vec<&OutputFile> = outputs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for output in sorted {
        update_len_prefixed(&mut hasher, output.name.as_bytes());
        hasher.update(output.sha256.as_bytes());
        hasher.update(output.size_bytes.to_le_bytes());
    }

    hex::encode(hasher.finalize())
}

/// 8 字节小端长度前缀 + 字段字节。
fn update_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::HashMap;

    fn sample_profile() -> EmbeddingProfile {
        EmbeddingProfile {
            model_id: "jina-embeddings-v2".to_string(),
            dim: 512,
        }
    }

    fn sample_codec() -> CodecMetadata {
        CodecMetadata {
            dim: 512,
            centroids_per_dim: 256,
            centroids_seed: 42,
            projection_seed: 7,
        }
    }

    fn sample_outputs() -> Vec<OutputFile> {
        vec![
            OutputFile {
                name: "centroids.bin".to_string(),
                sha256: "aa".repeat(32),
                size_bytes: 1024,
            },
            OutputFile {
                name: "records.turbo".to_string(),
                sha256: "bb".repeat(32),
                size_bytes: 4096,
            },
        ]
    }

    fn sample_manifest() -> ReleaseManifest {
        let outputs = sample_outputs();
        let content_digest = "cc".repeat(32);
        let release_id = derive_release_id(
            3,
            &sample_profile(),
            &sample_codec(),
            &content_digest,
            &outputs,
        );
        ReleaseManifest {
            manifest_schema_version: 1,
            turbo_version: 3,
            release_id,
            source: ReleaseSource {
                kind: "lance".to_string(),
                dataset_path: "/data/corpus.lance".to_string(),
                table_version: 9,
                table_row_count: 2,
                corpus_type: crate::models::CorpusType::Legal,
            },
            embedding_profile: sample_profile(),
            input_fingerprint: InputFingerprint {
                doc_count: 2,
                content_digest,
            },
            codec: sample_codec(),
            outputs,
        }
    }

    #[test]
    fn manifest_serializes_deterministically() {
        let m = sample_manifest();
        let bytes_a = serde_json::to_vec(&m).unwrap();
        let bytes_b = serde_json::to_vec(&m).unwrap();
        assert_eq!(bytes_a, bytes_b);

        let text = String::from_utf8(bytes_a).unwrap();
        assert!(
            !text.contains("timestamp"),
            "manifest must not contain timestamp"
        );
        assert!(!text.contains("uuid"), "manifest must not contain uuid");
    }

    #[test]
    fn release_id_is_content_derived_and_stable() {
        let profile = sample_profile();
        let codec = sample_codec();
        let digest = "cc".repeat(32);
        let outputs = sample_outputs();

        let id_a = derive_release_id(3, &profile, &codec, &digest, &outputs);
        let id_b = derive_release_id(3, &profile, &codec, &digest, &outputs);
        assert_eq!(id_a, id_b, "same input must yield same release_id");

        // dataset_path 属于 source，不参与 release_id：这里体现为 derive_release_id
        // 根本不接收 source 分量，因此改 dataset_path 无从影响结果。
        let mut m1 = sample_manifest();
        let mut m2 = sample_manifest();
        m1.source.dataset_path = "/disk-a/foo.lance".to_string();
        m2.source.dataset_path = "/disk-b/bar.lance".to_string();
        let rid1 = derive_release_id(
            m1.turbo_version,
            &m1.embedding_profile,
            &m1.codec,
            &m1.input_fingerprint.content_digest,
            &m1.outputs,
        );
        let rid2 = derive_release_id(
            m2.turbo_version,
            &m2.embedding_profile,
            &m2.codec,
            &m2.input_fingerprint.content_digest,
            &m2.outputs,
        );
        assert_eq!(
            rid1, rid2,
            "changing dataset_path (source) must not change release_id"
        );
    }

    #[test]
    fn release_id_changes_when_an_output_hash_changes() {
        let profile = sample_profile();
        let codec = sample_codec();
        let digest = "cc".repeat(32);
        let outputs = sample_outputs();

        let id_a = derive_release_id(3, &profile, &codec, &digest, &outputs);

        let mut changed = outputs.clone();
        changed[0].sha256 = "dd".repeat(32);
        let id_b = derive_release_id(3, &profile, &codec, &digest, &changed);

        assert_ne!(id_a, id_b, "changing an output hash must change release_id");
    }

    #[test]
    fn canonical_metadata_json_is_key_order_independent() {
        let mut a: HashMap<String, Value> = HashMap::new();
        a.insert("zebra".to_string(), json!(1));
        a.insert("alpha".to_string(), json!("x"));
        a.insert("mid".to_string(), json!([1, 2, 3]));

        let mut b: HashMap<String, Value> = HashMap::new();
        b.insert("mid".to_string(), json!([1, 2, 3]));
        b.insert("alpha".to_string(), json!("x"));
        b.insert("zebra".to_string(), json!(1));

        assert_eq!(
            canonical_metadata_json(&a),
            canonical_metadata_json(&b),
            "equivalent HashMaps with different insertion order must produce identical bytes"
        );
    }
}
