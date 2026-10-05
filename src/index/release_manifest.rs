//! TurboQuant static release(v3 与 v4)的自描述 manifest 与内容导出 release_id。
//!
//! 决定性构建的核心：manifest 中不含时间戳 / UUID / HashMap 序列化，
//! 因此 build-twice 逐字节相同。所有 digest / release_id 均为纯函数，可单测。
//!
//! v3 与 v4 共用同一个 manifest 结构，只有 `codec` 段不同([`ManifestCodec`])。
//! v3 manifest 的字节与 release_id 自 v4 引入后保持不变。

use super::header::{TURBO_VERSION_V3, TURBO_VERSION_V4};
use crate::models::CorpusType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// release manifest 在磁盘上的固定文件名。
pub const RELEASE_MANIFEST_FILE: &str = "release_manifest.json";

/// TurboQuant static release 的自描述 manifest。
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
    pub codec: ManifestCodec,
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

/// manifest 的 `codec` 段。两个版本的字段集不同，JSON 中不带 tag，
/// 靠字段集区分：v3 manifest 因此与引入 v4 之前逐字节相同。
///
/// 字段集本身不说明 release 是哪个版本；verify 层须核对它与
/// `turbo_version` 一致([`turbo_version`](Self::turbo_version))。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, expecting = "a v3 or v4 codec section")]
pub enum ManifestCodec {
    V4(V4CodecMetadata),
    V3(CodecMetadata),
}

impl ManifestCodec {
    pub fn dim(&self) -> u32 {
        match self {
            Self::V3(codec) => codec.dim,
            Self::V4(codec) => codec.dim,
        }
    }

    /// 该字段集所属的 `turbo_version`。
    pub fn turbo_version(&self) -> u32 {
        match self {
            Self::V3(_) => TURBO_VERSION_V3,
            Self::V4(_) => TURBO_VERSION_V4,
        }
    }
}

/// v3(`Legacy3BitV1`)codec 的决定性参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodecMetadata {
    pub dim: u32,
    pub centroids_per_dim: u32,
    pub centroids_seed: u64,
    pub projection_seed: u64,
}

/// v4 codec 的决定性参数：完整的
/// [`TurboQuantConfig`](super::TurboQuantConfig)，由
/// `TurboQuantConfig::{to,from}_v4_codec_metadata` 互转。
///
/// `codec_id` 与 `norm_policy` 存稳定的字符串名
/// (`TurboCodecId::name` / `NormPolicy::name`)，因此保持为 `String`：
/// 本构建不认识的名字能解析出来，再由 verify 层给出明确的拒绝理由。
/// 未知字段则直接拒绝：读不懂的 codec 参数意味着无法正确解码。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V4CodecMetadata {
    pub codec_id: String,
    pub dim: u32,
    pub mse_bits: u8,
    pub qjl_dim: u32,
    pub rotation_seed: u64,
    pub qjl_seed: u64,
    pub generator_version: u32,
    pub norm_policy: String,
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
/// 参与:`turbo_version` ∥ `profile`(model_id 长度前缀 + dim) ∥ `codec` ∥
/// `content_digest`(hex 字符串字节) ∥ 按 name 升序的 `outputs`
/// (name 长度前缀 + sha256 字节 + size_bytes)。
///
/// `codec` 分量按字段集而定:v3 为
/// dim/centroids_per_dim/centroids_seed/projection_seed;v4 为
/// codec_id(长度前缀)/dim/mse_bits/qjl_dim/rotation_seed/qjl_seed/
/// generator_version/norm_policy(长度前缀)，即完整的 codec config。
///
/// **排除整个 `source`**:同内容不同磁盘路径 → 同 release_id。
pub fn derive_release_id(
    turbo_version: u32,
    profile: &EmbeddingProfile,
    codec: &ManifestCodec,
    content_digest: &str,
    outputs: &[OutputFile],
) -> String {
    let mut hasher = Sha256::new();

    hasher.update(turbo_version.to_le_bytes());

    update_len_prefixed(&mut hasher, profile.model_id.as_bytes());
    hasher.update(profile.dim.to_le_bytes());

    match codec {
        ManifestCodec::V3(codec) => {
            hasher.update(codec.dim.to_le_bytes());
            hasher.update(codec.centroids_per_dim.to_le_bytes());
            hasher.update(codec.centroids_seed.to_le_bytes());
            hasher.update(codec.projection_seed.to_le_bytes());
        }
        ManifestCodec::V4(codec) => {
            update_len_prefixed(&mut hasher, codec.codec_id.as_bytes());
            hasher.update(codec.dim.to_le_bytes());
            hasher.update(codec.mse_bits.to_le_bytes());
            hasher.update(codec.qjl_dim.to_le_bytes());
            hasher.update(codec.rotation_seed.to_le_bytes());
            hasher.update(codec.qjl_seed.to_le_bytes());
            hasher.update(codec.generator_version.to_le_bytes());
            update_len_prefixed(&mut hasher, codec.norm_policy.as_bytes());
        }
    }

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

    fn sample_codec() -> ManifestCodec {
        ManifestCodec::V3(CodecMetadata {
            dim: 512,
            centroids_per_dim: 256,
            centroids_seed: 42,
            projection_seed: 7,
        })
    }

    fn sample_v4_codec() -> V4CodecMetadata {
        V4CodecMetadata {
            codec_id: "turbo_quant_prod_v1".to_string(),
            dim: 512,
            mse_bits: 2,
            qjl_dim: 512,
            rotation_seed: 163,
            qjl_seed: 165,
            generator_version: 1,
            norm_policy: "normalize_and_store".to_string(),
        }
    }

    fn sample_v4_manifest() -> ReleaseManifest {
        let codec = ManifestCodec::V4(sample_v4_codec());
        let mut manifest = sample_manifest();
        manifest.turbo_version = 4;
        manifest.release_id = derive_release_id(
            4,
            &manifest.embedding_profile,
            &codec,
            &manifest.input_fingerprint.content_digest,
            &manifest.outputs,
        );
        manifest.codec = codec;
        manifest
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

    /// `sample_manifest()` 在引入 v4 之前序列化出的字节。
    fn sample_v3_manifest_json() -> String {
        format!(
            concat!(
                r#"{{"manifest_schema_version":1,"turbo_version":3,"#,
                r#""release_id":"e72131f87d2d5635dff551d5679ee5d8697c69c64ef9c52ecb02c78bbefd2241","#,
                r#""source":{{"kind":"lance","dataset_path":"/data/corpus.lance","#,
                r#""table_version":9,"table_row_count":2,"corpus_type":"legal"}},"#,
                r#""embedding_profile":{{"model_id":"jina-embeddings-v2","dim":512}},"#,
                r#""input_fingerprint":{{"doc_count":2,"content_digest":"{digest}"}},"#,
                r#""codec":{{"dim":512,"centroids_per_dim":256,"centroids_seed":42,"#,
                r#""projection_seed":7}},"#,
                r#""outputs":[{{"name":"centroids.bin","sha256":"{aa}","size_bytes":1024}},"#,
                r#"{{"name":"records.turbo","sha256":"{bb}","size_bytes":4096}}]}}"#,
            ),
            digest = "cc".repeat(32),
            aa = "aa".repeat(32),
            bb = "bb".repeat(32),
        )
    }

    #[test]
    fn v3_manifest_bytes_and_release_id_are_unchanged() {
        let json = sample_v3_manifest_json();

        // release_id 按文档中的字节布局独立算出，不经过 `derive_release_id`。
        let manifest = sample_manifest();
        assert_eq!(
            manifest.release_id,
            "e72131f87d2d5635dff551d5679ee5d8697c69c64ef9c52ecb02c78bbefd2241"
        );
        assert_eq!(serde_json::to_string(&manifest).unwrap(), json);

        let parsed: ReleaseManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
        assert!(matches!(parsed.codec, ManifestCodec::V3(_)));
        assert_eq!(parsed.codec.turbo_version(), 3);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn v4_manifest_round_trips_with_the_full_codec_config() {
        let manifest = sample_v4_manifest();
        // 同样按文档布局独立算出。
        assert_eq!(
            manifest.release_id,
            "9f3dbc5a0872912dff7ee9272a80bb3bdf79f6f0c468b4401bbec394d1209669"
        );

        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            json.contains(concat!(
                r#""codec":{"codec_id":"turbo_quant_prod_v1","dim":512,"mse_bits":2,"#,
                r#""qjl_dim":512,"rotation_seed":163,"qjl_seed":165,"generator_version":1,"#,
                r#""norm_policy":"normalize_and_store"}"#,
            )),
            "{json}"
        );

        let parsed: ReleaseManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.codec, ManifestCodec::V4(sample_v4_codec()));
        assert_eq!(parsed.codec.turbo_version(), 4);
        assert_eq!(parsed.codec.dim(), 512);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn v4_release_id_covers_every_codec_field() {
        let manifest = sample_v4_manifest();
        let release_id = |codec: V4CodecMetadata| {
            derive_release_id(
                4,
                &manifest.embedding_profile,
                &ManifestCodec::V4(codec),
                &manifest.input_fingerprint.content_digest,
                &manifest.outputs,
            )
        };
        assert_eq!(release_id(sample_v4_codec()), manifest.release_id);

        type Edit = fn(&mut V4CodecMetadata);
        let edits: [(&str, Edit); 8] = [
            ("codec_id", |codec| codec.codec_id.push('x')),
            ("dim", |codec| codec.dim += 1),
            ("mse_bits", |codec| codec.mse_bits += 1),
            ("qjl_dim", |codec| codec.qjl_dim += 1),
            ("rotation_seed", |codec| codec.rotation_seed += 1),
            ("qjl_seed", |codec| codec.qjl_seed += 1),
            ("generator_version", |codec| codec.generator_version += 1),
            ("norm_policy", |codec| codec.norm_policy.push('x')),
        ];
        let mut seen = vec![manifest.release_id.clone()];
        for (field, edit) in edits {
            let mut codec = sample_v4_codec();
            edit(&mut codec);
            let id = release_id(codec);
            assert!(!seen.contains(&id), "{field} must change the release_id");
            seen.push(id);
        }
    }

    #[test]
    fn codec_section_that_is_neither_v3_nor_v4_is_rejected() {
        let v4_json = serde_json::to_string(&sample_v4_manifest()).unwrap();

        // 缺字段的 v4 段不会退化成 v3。
        let missing_field = v4_json.replace(r#""qjl_seed":165,"#, "");
        assert_ne!(missing_field, v4_json);
        let error = serde_json::from_str::<ReleaseManifest>(&missing_field).unwrap_err();
        assert!(
            error.to_string().contains("a v3 or v4 codec section"),
            "{error}"
        );

        // v4 段带未知字段：读不懂的 codec 参数不能被忽略。
        let unknown_field = v4_json.replace(r#""qjl_seed":165,"#, r#""qjl_seed":165,"extra":1,"#);
        assert!(serde_json::from_str::<ReleaseManifest>(&unknown_field).is_err());

        // 不认识的 codec 名能解析出来，由 verify 层拒绝。
        let unknown_codec = v4_json.replace("turbo_quant_prod_v1", "turbo_quant_prod_v9");
        let parsed: ReleaseManifest = serde_json::from_str(&unknown_codec).unwrap();
        let ManifestCodec::V4(codec) = parsed.codec else {
            panic!("expected a v4 codec section");
        };
        assert_eq!(codec.codec_id, "turbo_quant_prod_v9");
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
