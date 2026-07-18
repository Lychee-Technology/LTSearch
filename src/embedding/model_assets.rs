//! S3→/tmp 冷启动模型资产供给（#111）。
//!
//! Lambda ZIP 部署不携带模型：`scripts/package-model-assets.sh` 把 pinned ort
//! bundle 平铺上传到 S3 前缀（含 `manifest.json`，逐文件 sha256/bytes），两个
//! lambda bin 在启动期调用 [`provision_from_env`] 下载校验到
//! `LTSEARCH_{SIDE}_LTEMBED_BUNDLE_DIR`（生产为 `/tmp/ltembed`）。
//!
//! 完整性协议：数据文件先写 `.partial-*` 再 rename，`manifest.json` **最后**
//! 落盘作为完整性标记——warm 容器复用 `/tmp` 时凭本地 manifest + 尺寸快查跳过
//! 下载；因此绝不会把半套资产误判为就绪。

use std::env;
use std::fs;
use std::path::Path;

use aws_sdk_s3::config::ResponseChecksumValidation;
use serde::Deserialize;
use sha2::{Digest, Sha256};


pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAssetSource {
    pub bucket: String,
    pub prefix: String,
}

#[derive(Debug, Deserialize)]
pub struct AssetManifest {
    pub files: Vec<AssetFile>,
}

#[derive(Debug, Deserialize)]
pub struct AssetFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// 读 `LTSEARCH_{side}_LTEMBED_S3_BUCKET` / `..._S3_PREFIX`。都缺 = 未启用
/// S3 供给（资产由镜像/挂载预置），只设其一是配置错误。
pub fn model_asset_source_from_env(side: &str) -> Result<Option<ModelAssetSource>, String> {
    let bucket_var = format!("LTSEARCH_{side}_LTEMBED_S3_BUCKET");
    let prefix_var = format!("LTSEARCH_{side}_LTEMBED_S3_PREFIX");
    match (env::var(&bucket_var).ok(), env::var(&prefix_var).ok()) {
        (None, None) => Ok(None),
        (Some(bucket), Some(prefix)) => Ok(Some(ModelAssetSource {
            bucket,
            prefix: prefix.trim_matches('/').to_string(),
        })),
        (Some(_), None) => Err(format!("{bucket_var} is set but {prefix_var} is missing")),
        (None, Some(_)) => Err(format!("{prefix_var} is set but {bucket_var} is missing")),
    }
}

fn to_hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// warm 快查：本地 manifest 可解析且所有文件尺寸匹配即视为就绪
/// （manifest 最后写入，存在即代表上一次 provision 完整结束）。
fn assets_ready(bundle_dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string(bundle_dir.join(MANIFEST_FILE)) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<AssetManifest>(&text) else {
        return false;
    };
    manifest.files.iter().all(|file| {
        fs::metadata(bundle_dir.join(&file.name))
            .map(|meta| meta.len() == file.bytes)
            .unwrap_or(false)
    })
}

async fn get_object(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> Result<Vec<u8>, String> {
    let output = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|error| {
            format!(
                "model assets not provisioned: GET s3://{bucket}/{key} failed: {}",
                aws_sdk_s3::error::DisplayErrorContext(&error)
            )
        })?;
    let data = output.body.collect().await.map_err(|error| {
        format!("model assets not provisioned: reading s3://{bucket}/{key} body failed: {error:?}")
    })?;
    Ok(data.into_bytes().to_vec())
}

/// 大文件(model.ort ~118MB)流式落盘:边读边写边哈希,不整块驻留内存;
/// 返回 (bytes, sha256_hex)。
async fn download_to_file(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    dest: &Path,
) -> Result<(u64, String), String> {
    use std::io::Write;

    let output = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|error| {
            format!(
                "model assets not provisioned: GET s3://{bucket}/{key} failed: {}",
                aws_sdk_s3::error::DisplayErrorContext(&error)
            )
        })?;
    let mut body = output.body;
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::create(dest).map_err(|error| {
        format!("model assets not provisioned: creating {}: {error}", dest.display())
    })?;
    let mut total: u64 = 0;
    while let Some(chunk) = body.try_next().await.map_err(|error| {
        format!(
            "model assets not provisioned: streaming s3://{bucket}/{key} after {total} bytes: {error:?}"
        )
    })? {
        total += chunk.len() as u64;
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|error| {
            format!("model assets not provisioned: writing {}: {error}", dest.display())
        })?;
    }
    Ok((total, to_hex(&hasher.finalize())))
}

pub async fn provision_model_assets(
    client: &aws_sdk_s3::Client,
    source: &ModelAssetSource,
    bundle_dir: &str,
) -> Result<(), String> {
    let dir = Path::new(bundle_dir);
    if assets_ready(dir) {
        eprintln!("model assets: warm reuse of {bundle_dir}");
        return Ok(());
    }
    eprintln!(
        "model assets: provisioning s3://{}/{} -> {bundle_dir}",
        source.bucket, source.prefix
    );
    fs::create_dir_all(dir)
        .map_err(|error| format!("model assets not provisioned: mkdir {bundle_dir}: {error}"))?;

    let manifest_key = format!("{}/{MANIFEST_FILE}", source.prefix);
    let manifest_bytes = get_object(client, &source.bucket, &manifest_key).await?;
    let manifest: AssetManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        format!(
            "model assets not provisioned: s3://{}/{manifest_key} is not a valid manifest: {error}",
            source.bucket
        )
    })?;

    for file in &manifest.files {
        let key = format!("{}/{}", source.prefix, file.name);
        let partial = dir.join(format!(".partial-{}", file.name));
        let (bytes, actual) = download_to_file(client, &source.bucket, &key, &partial).await?;
        if bytes != file.bytes {
            return Err(format!(
                "model assets not provisioned: s3://{}/{key} is {bytes} bytes, manifest says {}",
                source.bucket, file.bytes
            ));
        }
        if actual != file.sha256 {
            return Err(format!(
                "model assets not provisioned: s3://{}/{key} sha256 {actual} does not match manifest {}",
                source.bucket, file.sha256
            ));
        }
        fs::rename(&partial, dir.join(&file.name)).map_err(|error| {
            format!("model assets not provisioned: staging {}: {error}", file.name)
        })?;
    }

    // manifest 最后写入 = 完整性标记。
    fs::write(dir.join(MANIFEST_FILE), &manifest_bytes).map_err(|error| {
        format!("model assets not provisioned: writing {MANIFEST_FILE}: {error}")
    })?;
    eprintln!(
        "model assets: provisioned {} files into {bundle_dir}",
        manifest.files.len()
    );
    Ok(())
}

/// bin 启动期入口：provider 非 ltembed 或未配置 S3 供给时为 no-op。
pub async fn provision_from_env(side: &str) -> Result<(), String> {
    let provider_var = format!("LTSEARCH_{side}_EMBEDDING_PROVIDER");
    if env::var(&provider_var).as_deref() != Ok("ltembed") {
        return Ok(());
    }
    let Some(source) = model_asset_source_from_env(side)? else {
        return Ok(());
    };
    let bundle_dir_var = format!("LTSEARCH_{side}_LTEMBED_BUNDLE_DIR");
    let bundle_dir = env::var(&bundle_dir_var).map_err(|_| {
        format!("LTSEARCH_{side}_LTEMBED_S3_BUCKET is set but {bundle_dir_var} is missing")
    })?;
    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    provision_model_assets(&provisioning_s3_client(&sdk_config), &source, &bundle_dir).await
}

/// 供给专用 S3 client：跳过可选的响应 CRC 校验（`WhenRequired`）——完整性由
/// manifest 的逐文件 sha256 保证（强于 CRC），且 moto 对 multipart 上传对象
/// 返回的复合 checksum 会让 SDK 默认校验在大文件（model.ort）上误报
/// ChecksumMismatch。endpoint 处理与 bootstrap::s3_client_from_env 对齐。
fn provisioning_s3_client(config: &aws_config::SdkConfig) -> aws_sdk_s3::Client {
    let mut builder = aws_sdk_s3::config::Builder::from(config)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
    if let Ok(endpoint_url) = env::var("AWS_ENDPOINT_URL_S3") {
        builder = builder.endpoint_url(endpoint_url).force_path_style(true);
    }
    aws_sdk_s3::Client::from_conf(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_from_env_absent_pair_is_none() {
        env::remove_var("LTSEARCH_MA_NONE_LTEMBED_S3_BUCKET");
        env::remove_var("LTSEARCH_MA_NONE_LTEMBED_S3_PREFIX");
        assert_eq!(model_asset_source_from_env("MA_NONE").unwrap(), None);
    }

    #[test]
    fn source_from_env_partial_pair_is_config_error() {
        env::set_var("LTSEARCH_MA_PART_LTEMBED_S3_BUCKET", "bucket");
        env::remove_var("LTSEARCH_MA_PART_LTEMBED_S3_PREFIX");
        let error = model_asset_source_from_env("MA_PART").unwrap_err();
        assert!(error.contains("LTSEARCH_MA_PART_LTEMBED_S3_PREFIX"), "{error}");
    }

    #[test]
    fn source_from_env_trims_prefix_slashes() {
        env::set_var("LTSEARCH_MA_FULL_LTEMBED_S3_BUCKET", "bucket");
        env::set_var("LTSEARCH_MA_FULL_LTEMBED_S3_PREFIX", "/ltembed/v1.0.9/");
        let source = model_asset_source_from_env("MA_FULL").unwrap().unwrap();
        assert_eq!(source.prefix, "ltembed/v1.0.9");
    }

    #[test]
    fn sha256_to_hex_matches_known_vector() {
        assert_eq!(
            to_hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn assets_ready_requires_manifest_and_matching_sizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!assets_ready(dir.path()));

        fs::write(dir.path().join("model.ort"), b"weights").unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            r#"{"files":[{"name":"model.ort","bytes":7,"sha256":"unused-for-warm-check"}]}"#,
        )
        .unwrap();
        assert!(assets_ready(dir.path()));

        // 尺寸不符（截断/损坏）必须判定未就绪。
        fs::write(dir.path().join("model.ort"), b"tr").unwrap();
        assert!(!assets_ready(dir.path()));
    }
}
