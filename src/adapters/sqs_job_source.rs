//! 构建作业消费侧的 AWS 实现：`receive` 长轮询取 1 条消息（等待 10s），`ack`
//! 按 receipt handle `delete_message`。worker 循环只依赖中立的 [`BuildJobSource`]，
//! SQS 细节收敛于此，`#[cfg(feature = "aws")]` 门控。

use async_trait::async_trait;
use aws_sdk_sqs::Client as SqsClient;

use crate::contracts::{BuildJob, BuildJobSource};

#[derive(Clone)]
pub struct SqsBuildJobSource {
    client: SqsClient,
    queue_url: String,
}

impl SqsBuildJobSource {
    pub fn new(client: SqsClient, queue_url: impl Into<String>) -> Self {
        Self {
            client,
            queue_url: queue_url.into(),
        }
    }
}

#[async_trait]
impl BuildJobSource for SqsBuildJobSource {
    async fn receive(&self) -> Result<Vec<BuildJob>, String> {
        let output = self
            .client
            .receive_message()
            .queue_url(&self.queue_url)
            .max_number_of_messages(1)
            .wait_time_seconds(10)
            .send()
            .await
            .map_err(|error| format!("receive_message failed: {error}"))?;

        let jobs = output
            .messages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|message| {
                // 无 receipt handle 无法 ack，跳过（正常 SQS 消息必带）。
                let receipt = message.receipt_handle()?.to_string();
                let body = message.body().unwrap_or_default().to_string();
                Some(BuildJob { receipt, body })
            })
            .collect();
        Ok(jobs)
    }

    async fn ack(&self, job: &BuildJob) -> Result<(), String> {
        self.client
            .delete_message()
            .queue_url(&self.queue_url)
            .receipt_handle(&job.receipt)
            .send()
            .await
            .map_err(|error| format!("delete_message failed: {error}"))?;
        Ok(())
    }
}
