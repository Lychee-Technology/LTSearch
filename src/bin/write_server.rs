use std::sync::Arc;

use futures::future::FutureExt;

use ltsearch::adapters::s3_wal::AwsS3WalStorage;
use ltsearch::adapters::sqs_build_queue::AwsSqsBuildQueue;
use ltsearch::bootstrap::{s3_client_from_env, sqs_client_from_env, WriteConfig};
use ltsearch::http::write::{write_router, WriteServerState};
use ltsearch::http::{port_from_env, serve};
use ltsearch::write::api::WriteApi;
use ltsearch::write::wal::WriteAheadLog;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async {
        // 接线照抄 src/bin/write_lambda.rs:56-63。
        let write_config = WriteConfig::from_env()?;
        let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;

        let wal_storage =
            AwsS3WalStorage::new(write_config.s3_bucket, s3_client_from_env(&sdk_config));
        let build_queue =
            AwsSqsBuildQueue::new(write_config.sqs_queue_url, sqs_client_from_env(&sdk_config));
        let write_api = Arc::new(WriteApi::new(WriteAheadLog::new(wal_storage), build_queue));

        let ingest_api = write_api.clone();
        let delete_api = write_api.clone();
        let state = WriteServerState {
            ingest: Arc::new(move |documents| {
                let api = ingest_api.clone();
                async move { api.ingest(documents).await }.boxed()
            }),
            delete: Arc::new(move |doc_ids| {
                let api = delete_api.clone();
                async move { api.delete(doc_ids).await }.boxed()
            }),
        };

        let port = port_from_env();
        eprintln!("ltsearch-write-server listening on 0.0.0.0:{port}");
        serve(write_router(state), port).await?;
        Ok(())
    })
}
