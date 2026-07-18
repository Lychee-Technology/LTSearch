//! S3→/tmp 冷启动模型资产供给（#111）。
//!
//! Lambda ZIP 部署不携带模型：`scripts/package-model-assets.sh` 把 pinned ort
//! bundle 平铺上传到 S3 前缀（含 `manifest.json`，逐文件 sha256/bytes），两个
//! lambda bin 在启动期调用 [`provision_from_env`] 下载校验到
//! `LTSEARCH_{SIDE}_LTEMBED_BUNDLE_DIR`（生产为 `/tmp/ltembed`）。S3 client 由
//! 调用方注入（composition root 在 `bootstrap::model_assets_s3_client_from_env`）。
//!
//! 完整性协议：数据文件先写 `.partial-*` 再 rename，`manifest.json` **最后**
//! 落盘作为完整性标记——warm 容器复用 `/tmp` 时凭本地 manifest + 尺寸快查跳过
//! 下载。refresh（快查未通过）**先删除旧 manifest 再动任何文件**：部分替换后
//! 失败的目录不会残留完整性标记，重试必然全量重下，绝不会把同尺寸新旧混合的
//! bundle 误判为就绪。

use std::env;
use std::fs;
use std::path::Path;

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
/// （manifest 最后写入且 refresh 先删，存在即代表上一次 provision 完整结束）。
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

/// 资产取回抽象：生产实现走 S3（[`S3Fetcher`]），测试注入假 fetcher 模拟
/// 部分失败的 refresh。
trait AssetFetcher {
    /// 人类可读的来源标识（日志/错误信息用）。
    fn label(&self) -> String;
    /// 小文件整块取回（manifest）。
    async fn fetch_bytes(&self, name: &str) -> Result<Vec<u8>, String>;
    /// 数据文件流式落盘，返回 (bytes, sha256_hex)。
    async fn fetch_to_file(&self, name: &str, dest: &Path) -> Result<(u64, String), String>;
}

struct S3Fetcher<'a> {
    client: &'a aws_sdk_s3::Client,
    source: &'a ModelAssetSource,
}

impl S3Fetcher<'_> {
    fn key(&self, name: &str) -> String {
        format!("{}/{name}", self.source.prefix)
    }

    fn uri(&self, name: &str) -> String {
        format!("s3://{}/{}", self.source.bucket, self.key(name))
    }
}

impl AssetFetcher for S3Fetcher<'_> {
    fn label(&self) -> String {
        format!("s3://{}/{}", self.source.bucket, self.source.prefix)
    }

    async fn fetch_bytes(&self, name: &str) -> Result<Vec<u8>, String> {
        let output = send_get(
            self.client,
            &self.source.bucket,
            &self.key(name),
            &self.uri(name),
        )
        .await?;
        let data = output.body.collect().await.map_err(|error| {
            format!(
                "model assets not provisioned: reading {} body failed: {error:?}",
                self.uri(name)
            )
        })?;
        Ok(data.into_bytes().to_vec())
    }

    /// 大文件（model.ort ~118MB）流式落盘：边读边写边哈希，不整块驻留内存。
    async fn fetch_to_file(&self, name: &str, dest: &Path) -> Result<(u64, String), String> {
        use std::io::Write;

        let output = send_get(
            self.client,
            &self.source.bucket,
            &self.key(name),
            &self.uri(name),
        )
        .await?;
        let mut body = output.body;
        let mut hasher = Sha256::new();
        let mut file = std::fs::File::create(dest).map_err(|error| {
            format!(
                "model assets not provisioned: creating {}: {error}",
                dest.display()
            )
        })?;
        let mut total: u64 = 0;
        while let Some(chunk) = body.try_next().await.map_err(|error| {
            format!(
                "model assets not provisioned: streaming {} after {total} bytes: {error:?}",
                self.uri(name)
            )
        })? {
            total += chunk.len() as u64;
            hasher.update(&chunk);
            file.write_all(&chunk).map_err(|error| {
                format!(
                    "model assets not provisioned: writing {}: {error}",
                    dest.display()
                )
            })?;
        }
        Ok((total, to_hex(&hasher.finalize())))
    }
}

async fn send_get(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    uri: &str,
) -> Result<aws_sdk_s3::operation::get_object::GetObjectOutput, String> {
    client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|error| {
            format!(
                "model assets not provisioned: GET {uri} failed: {}",
                aws_sdk_s3::error::DisplayErrorContext(&error)
            )
        })
}

pub async fn provision_model_assets(
    client: &aws_sdk_s3::Client,
    source: &ModelAssetSource,
    bundle_dir: &str,
) -> Result<(), String> {
    provision_with(&S3Fetcher { client, source }, bundle_dir).await
}

async fn provision_with<F: AssetFetcher>(fetcher: &F, bundle_dir: &str) -> Result<(), String> {
    let dir = Path::new(bundle_dir);
    if assets_ready(dir) {
        eprintln!("model assets: warm reuse of {bundle_dir}");
        return Ok(());
    }
    eprintln!(
        "model assets: provisioning {} -> {bundle_dir}",
        fetcher.label()
    );
    fs::create_dir_all(dir)
        .map_err(|error| format!("model assets not provisioned: mkdir {bundle_dir}: {error}"))?;
    // refresh 前先作废完整性标记：若本次部分替换文件后失败，残留目录不带
    // manifest，重试的尺寸快查必然 miss，走全量重下——避免同尺寸新旧文件
    // 混合的 bundle 被误判为就绪而跳过 sha256 校验。
    let marker = dir.join(MANIFEST_FILE);
    if marker.exists() {
        fs::remove_file(&marker).map_err(|error| {
            format!("model assets not provisioned: invalidating stale {MANIFEST_FILE}: {error}")
        })?;
    }

    let manifest_bytes = fetcher.fetch_bytes(MANIFEST_FILE).await?;
    let manifest: AssetManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        format!(
            "model assets not provisioned: {}/{MANIFEST_FILE} is not a valid manifest: {error}",
            fetcher.label()
        )
    })?;

    for file in &manifest.files {
        let partial = dir.join(format!(".partial-{}", file.name));
        let (bytes, actual) = fetcher.fetch_to_file(&file.name, &partial).await?;
        if bytes != file.bytes {
            return Err(format!(
                "model assets not provisioned: {}/{} is {bytes} bytes, manifest says {}",
                fetcher.label(),
                file.name,
                file.bytes
            ));
        }
        if actual != file.sha256 {
            return Err(format!(
                "model assets not provisioned: {}/{} sha256 {actual} does not match manifest {}",
                fetcher.label(),
                file.name,
                file.sha256
            ));
        }
        fs::rename(&partial, dir.join(&file.name)).map_err(|error| {
            format!(
                "model assets not provisioned: staging {}: {error}",
                file.name
            )
        })?;
    }

    // manifest 最后写入 = 完整性标记。
    fs::write(&marker, &manifest_bytes).map_err(|error| {
        format!("model assets not provisioned: writing {MANIFEST_FILE}: {error}")
    })?;
    eprintln!(
        "model assets: provisioned {} files into {bundle_dir}",
        manifest.files.len()
    );
    Ok(())
}

/// bin 启动期入口：provider 非 ltembed 或未配置 S3 供给时为 no-op。
/// client 由调用方经 `bootstrap::model_assets_s3_client_from_env` 构造注入。
pub async fn provision_from_env(side: &str, client: &aws_sdk_s3::Client) -> Result<(), String> {
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
    provision_model_assets(client, &source, &bundle_dir).await
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

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
        assert!(
            error.contains("LTSEARCH_MA_PART_LTEMBED_S3_PREFIX"),
            "{error}"
        );
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

    /// 测试 fetcher：内存文件表，`fail_after_files` 个数据文件后开始报错，
    /// 模拟 refresh 中途断网。
    struct FakeFetcher {
        files: HashMap<&'static str, Vec<u8>>,
        fail_after_files: Option<usize>,
        fetched: RefCell<usize>,
    }

    impl FakeFetcher {
        fn new(entries: &[(&'static str, &[u8])], fail_after_files: Option<usize>) -> Self {
            let files: HashMap<&'static str, Vec<u8>> = entries
                .iter()
                .map(|(name, data)| (*name, data.to_vec()))
                .collect();
            Self {
                files,
                fail_after_files,
                fetched: RefCell::new(0),
            }
        }

        fn manifest_json(entries: &[(&'static str, &[u8])]) -> Vec<u8> {
            let files: Vec<String> = entries
                .iter()
                .map(|(name, data)| {
                    format!(
                        r#"{{"name":"{name}","bytes":{},"sha256":"{}"}}"#,
                        data.len(),
                        to_hex(&Sha256::digest(data))
                    )
                })
                .collect();
            format!(r#"{{"files":[{}]}}"#, files.join(",")).into_bytes()
        }
    }

    impl AssetFetcher for FakeFetcher {
        fn label(&self) -> String {
            "fake://bundle".to_string()
        }

        async fn fetch_bytes(&self, name: &str) -> Result<Vec<u8>, String> {
            self.files
                .get(name)
                .cloned()
                .ok_or_else(|| format!("fake fetcher has no {name}"))
        }

        async fn fetch_to_file(&self, name: &str, dest: &Path) -> Result<(u64, String), String> {
            let mut fetched = self.fetched.borrow_mut();
            if let Some(limit) = self.fail_after_files {
                if *fetched >= limit {
                    return Err(format!("fake network failure before {name}"));
                }
            }
            *fetched += 1;
            let data = self
                .files
                .get(name)
                .cloned()
                .ok_or_else(|| format!("fake fetcher has no {name}"))?;
            fs::write(dest, &data).map_err(|error| error.to_string())?;
            Ok((data.len() as u64, to_hex(&Sha256::digest(&data))))
        }
    }

    fn fake_bundle(
        model: &'static [u8],
        tokenizer: &'static [u8],
    ) -> Vec<(&'static str, &'static [u8])> {
        vec![("model.ort", model), ("tokenizer.json", tokenizer)]
    }

    #[tokio::test]
    async fn provision_writes_manifest_last_and_reaches_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_dir = dir.path().to_str().unwrap();
        let entries = fake_bundle(b"weights-v1", b"tok-v1");
        let mut fetcher = FakeFetcher::new(&entries, None);
        fetcher
            .files
            .insert(MANIFEST_FILE, FakeFetcher::manifest_json(&entries));

        provision_with(&fetcher, bundle_dir)
            .await
            .expect("provision");
        assert!(assets_ready(dir.path()));
        assert_eq!(
            fs::read(dir.path().join("model.ort")).unwrap(),
            b"weights-v1"
        );
    }

    /// P1 回归（PR #137 review）：refresh 中途失败不得残留旧 manifest——
    /// 否则同尺寸新旧文件混合的 bundle 会被尺寸快查误判为就绪、跳过 sha256。
    #[tokio::test]
    async fn failed_refresh_invalidates_marker_and_retry_redownloads_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_dir = dir.path().to_str().unwrap();

        // 第一轮：v1 bundle 完整就位。
        let v1 = fake_bundle(b"weights-v1", b"tok-v1");
        let mut fetcher_v1 = FakeFetcher::new(&v1, None);
        fetcher_v1
            .files
            .insert(MANIFEST_FILE, FakeFetcher::manifest_json(&v1));
        provision_with(&fetcher_v1, bundle_dir).await.expect("v1");
        assert!(assets_ready(dir.path()));

        // 强制 refresh：删掉首个文件（model.ort），让尺寸快查 miss。
        fs::remove_file(dir.path().join("model.ort")).unwrap();
        assert!(!assets_ready(dir.path()));

        // 第二轮：v2 内容不同但**逐文件尺寸与 v1 相同**；重下完第 1 个文件
        // （model.ort→v2）后失败。若旧 manifest 不先作废，此时目录内
        // model.ort（v2）与 tokenizer.json（v1）的尺寸恰好全部匹配旧 manifest，
        // 尺寸快查会把混合 bundle 误判为就绪——本用例在旧实现下必然失败。
        let v2 = fake_bundle(b"weights-v2", b"tok-v2");
        let mut fetcher_v2_fail = FakeFetcher::new(&v2, Some(1));
        fetcher_v2_fail
            .files
            .insert(MANIFEST_FILE, FakeFetcher::manifest_json(&v2));
        let error = provision_with(&fetcher_v2_fail, bundle_dir)
            .await
            .expect_err("mid-refresh failure");
        assert!(error.contains("fake network failure"), "{error}");

        // 关键断言：完整性标记已作废，混合目录绝不就绪。
        assert!(!dir.path().join(MANIFEST_FILE).exists());
        assert!(!assets_ready(dir.path()));

        // 第三轮重试：全量重下成功，v2 内容完整就位。
        let mut fetcher_v2 = FakeFetcher::new(&v2, None);
        fetcher_v2
            .files
            .insert(MANIFEST_FILE, FakeFetcher::manifest_json(&v2));
        provision_with(&fetcher_v2, bundle_dir)
            .await
            .expect("retry");
        assert!(assets_ready(dir.path()));
        assert_eq!(
            fs::read(dir.path().join("model.ort")).unwrap(),
            b"weights-v2"
        );
        assert_eq!(
            fs::read(dir.path().join("tokenizer.json")).unwrap(),
            b"tok-v2"
        );
    }
}
