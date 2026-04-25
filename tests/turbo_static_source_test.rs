use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use ltsearch::index::{load_static_chunks_from_s3, StaticSourceConfig, TurboBuildConfig};
use ltsearch::models::CorpusType;

#[test]
fn turbo_build_config_deserializes_static_sources_with_corpus_type() {
    let config: TurboBuildConfig = serde_json::from_str(
        r#"{
            "sources": [
                {
                    "bucket": "static-bucket",
                    "key": "static/legal.jsonl",
                    "corpus_type": "legal"
                },
                {
                    "bucket": "static-bucket",
                    "key": "static/rfc.jsonl",
                    "corpus_type": "rfc"
                }
            ]
        }"#,
    )
    .unwrap();

    assert_eq!(
        config,
        TurboBuildConfig {
            sources: vec![
                StaticSourceConfig {
                    bucket: "static-bucket".into(),
                    key: "static/legal.jsonl".into(),
                    corpus_type: CorpusType::Legal,
                },
                StaticSourceConfig {
                    bucket: "static-bucket".into(),
                    key: "static/rfc.jsonl".into(),
                    corpus_type: CorpusType::Rfc,
                },
            ],
        }
    );
}

#[tokio::test]
async fn load_static_chunks_from_s3_reads_jsonl_and_applies_source_corpus_type() {
    let harness = MotoHarness::new("static-source-load").await;
    harness
        .put_object(
            "static/contracts.jsonl",
            concat!(
                r#"{"doc_id":"doc-1","text":"contract one","metadata":{"lang":"en","version":1}}"#,
                "\n",
                r#"{"doc_id":"doc-2","text":"contract two","metadata":{"lang":"fr","active":true}}"#,
                "\n"
            ),
        )
        .await;

    let chunks = load_static_chunks_from_s3(
        &harness.s3,
        &[StaticSourceConfig {
            bucket: harness.bucket.clone(),
            key: "static/contracts.jsonl".into(),
            corpus_type: CorpusType::Contract,
        }],
    )
    .await
    .unwrap();

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].doc_id, "doc-1");
    assert_eq!(chunks[0].text, "contract one");
    assert_eq!(chunks[0].corpus_type, CorpusType::Contract);
    assert_eq!(chunks[0].metadata["lang"], serde_json::json!("en"));
    assert_eq!(chunks[0].metadata["version"], serde_json::json!(1));
    assert_eq!(chunks[1].doc_id, "doc-2");
    assert_eq!(chunks[1].text, "contract two");
    assert_eq!(chunks[1].corpus_type, CorpusType::Contract);
    assert_eq!(chunks[1].metadata["lang"], serde_json::json!("fr"));
    assert_eq!(chunks[1].metadata["active"], serde_json::json!(true));
}

struct MotoHarness {
    bucket: String,
    s3: S3Client,
}

impl MotoHarness {
    async fn new(name: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let bucket = format!("ltsearch-{name}-{suffix}").to_lowercase();
        let credentials = Credentials::new("test", "test", None, None, "moto");
        let region = Region::new("us-east-1");

        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(region)
            .credentials_provider(credentials)
            .endpoint_url("http://localhost:5000")
            .load()
            .await;

        let s3_config = aws_sdk_s3::config::Builder::from(&shared_config)
            .force_path_style(true)
            .build();
        let s3 = S3Client::from_conf(s3_config);

        wait_until_bucket_ready(&s3, &bucket).await;

        Self { bucket, s3 }
    }

    async fn put_object(&self, key: &str, body: &str) {
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body.as_bytes().to_vec()))
            .send()
            .await
            .unwrap();
    }
}

async fn wait_until_bucket_ready(s3: &S3Client, bucket: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);

    loop {
        match s3.create_bucket().bucket(bucket).send().await {
            Ok(_) => return,
            Err(error) => {
                if s3.head_bucket().bucket(bucket).send().await.is_ok() {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    panic!("Moto did not become ready: bucket={error:?}");
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
