//! 供应商中立契约门面（provider-neutral contracts）。
//!
//! #107 的核心不引入新抽象，而是把已经存在、且不含任何基础设施类型的四个契约
//! 收敛到一个入口，并补齐两个尚缺的消费侧契约，使 domain core 在没有 AWS 的前提
//! 下也能被完整构造。四类契约对应 issue 的四个语义：
//!
//! - 文档事件（document events）→ [`WalStorage`]
//! - 构建作业（build jobs）→ [`BuildQueue`]（生产侧）+ [`BuildJobSource`]（消费侧）
//! - 制品访问（artifact access）→ [`PublishStorage`]（读写）+ [`ArtifactSync`]（查询侧下载）
//! - 活跃版本协调（active-release coordination）→ [`ManifestStore`]

use async_trait::async_trait;
use std::path::Path;

pub use crate::indexing::PublishStorage;
pub use crate::storage::ManifestStore;
pub use crate::write::{BuildQueue, WalStorage};

/// 构建队列上的一条待处理作业：`receipt` 是删除/确认所需的句柄（SQS receipt
/// handle 或本地文件名），`body` 是原始 JSON（`QueueBatch` 的序列化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildJob {
    pub receipt: String,
    pub body: String,
}

/// 构建作业消费侧契约：worker 轮询循环只依赖它，不再直接触碰 SQS。AWS 实现见
/// `#[cfg(feature = "aws")]` 的 `SqsBuildJobSource`，本地实现见 `LocalFsBuildQueue`。
#[async_trait]
pub trait BuildJobSource: Send + Sync {
    /// 拉取零个或多个待处理作业（长轮询实现可阻塞至超时）。
    async fn receive(&self) -> Result<Vec<BuildJob>, String>;
    /// 处理完成后确认（删除）一条作业，无论处理成功与否都应调用。
    async fn ack(&self, job: &BuildJob) -> Result<(), String>;
}

/// 查询侧制品访问契约：把活跃版本所需的 index/lance/static 制品同步到本地
/// `artifact_root`。AWS 实现从 S3 下载前缀；本地实现（制品已在盘上）是 no-op。
#[async_trait]
pub trait ArtifactSync: Send + Sync {
    async fn sync(&self, artifact_root: &Path) -> Result<(), String>;
}
