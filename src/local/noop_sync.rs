//! 本地制品同步：制品已在盘上（挂载卷或本地构建产出），无需下载，`sync` 为
//! no-op。保留契约以便 query 侧代码在 local / aws 之间只换实现不换调用点。

use std::path::Path;

use async_trait::async_trait;

use crate::contracts::ArtifactSync;

#[derive(Debug, Clone, Default)]
pub struct NoopArtifactSync;

impl NoopArtifactSync {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ArtifactSync for NoopArtifactSync {
    async fn sync(&self, _artifact_root: &Path) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_sync_returns_ok_without_touching_disk() {
        let sync = NoopArtifactSync::new();
        sync.sync(Path::new("/definitely/not/created"))
            .await
            .unwrap();
    }
}
