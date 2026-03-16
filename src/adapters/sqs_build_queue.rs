use async_trait::async_trait;
use aws_sdk_sqs::Client as SqsClient;

use crate::error::IngestError;
use crate::write::{BuildQueue, QueueBatch};

#[derive(Clone)]
pub struct AwsSqsBuildQueue {
    queue_url: String,
    client: SqsClient,
}

impl AwsSqsBuildQueue {
    pub fn new(queue_url: impl Into<String>, client: SqsClient) -> Self {
        Self {
            queue_url: queue_url.into(),
            client,
        }
    }

    pub fn queue_url(&self) -> &str {
        &self.queue_url
    }

    pub fn client(&self) -> &SqsClient {
        &self.client
    }
}

#[async_trait]
impl BuildQueue for AwsSqsBuildQueue {
    async fn enqueue(&self, batch: QueueBatch) -> Result<(), IngestError> {
        let message = serde_json::to_string(&batch).map_err(|error| IngestError::Operation {
            message: format!("failed to serialize queue batch: {error}"),
        })?;

        self.client
            .send_message()
            .queue_url(&self.queue_url)
            .message_body(message)
            .send()
            .await
            .map_err(|error| IngestError::Operation {
                message: format!("failed to enqueue queue batch: {error}"),
            })?;

        Ok(())
    }
}
