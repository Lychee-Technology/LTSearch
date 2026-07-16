//! 本地（AWS-free）契约实现。这些类型只依赖 std / tokio / 文件系统，可在任何
//! profile 下编译；`local` profile 用它们构造 runtime，#108 将以 SQLite 版本
//! 替换耐久事件与版本协调部分。

pub mod fs_build_queue;
pub mod fs_publish;
pub mod fs_wal;
pub mod noop_sync;
#[cfg(feature = "local")]
pub mod sqlite;

pub use fs_build_queue::LocalFsBuildQueue;
pub use fs_publish::LocalFsPublishStorage;
pub use fs_wal::LocalFsWalStorage;
pub use noop_sync::NoopArtifactSync;
#[cfg(feature = "local")]
pub use sqlite::{
    LocalPublishStorage, SqliteBuildJobSource, SqliteBuildQueue, SqliteDb, SqliteManifestStore,
    SqliteWalStorage,
};
