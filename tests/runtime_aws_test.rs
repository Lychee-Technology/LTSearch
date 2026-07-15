//! aws profile 构造证明：用 AWS 适配器组装出与 local 对应的四类契约实现，断言
//! 构造成功（仅构造，不做网络 I/O）。
#![cfg(feature = "aws")]

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use ltsearch::adapters::s3_artifact_sync::S3ArtifactSync;
use ltsearch::adapters::s3_publish::AwsPublishStorage;
use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::adapters::sqs_job_source::SqsBuildJobSource;

#[tokio::test]
async fn aws_profile_constructs_all_adapter_types() {
    let credentials = Credentials::new("test", "test", None, None, "runtime-proof");
    let shared_config = aws_config::defaults(BehaviorVersion::latest())
        .credentials_provider(credentials)
        .region(Region::new("us-east-1"))
        .load()
        .await;
    let s3 = S3Client::new(&shared_config);
    let sqs = SqsClient::new(&shared_config);

    // document events + artifact access (read/write)
    let _wal = AwsS3WalStorage::new("bucket", s3.clone());
    let _publish = AwsPublishStorage::new("bucket", s3);
    // build jobs: producer + consumer
    let _queue = AwsSqsBuildQueue::new("http://queue", sqs.clone());
    let _job_source = SqsBuildJobSource::new(sqs, "http://queue");
    // artifact access: query-side download
    let _artifact_sync = S3ArtifactSync::new("bucket");
}
