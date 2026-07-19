//! 本地（AWS-free）契约实现。耐久文档事件日志、构建作业队列与活跃发布指针由 SQLite
//! 承载（`sqlite` 子模块，#108）；不可变制品的字节读写仍走文件系统
//! （[`LocalFsPublishStorage`]，被 SQLite 混合发布存储复用），查询侧下载为 no-op
//! （[`NoopArtifactSync`]）。#116 引入的文件型 WAL/队列实现已随 SQLite 切换退役。

pub mod fs_publish;
pub mod noop_sync;
#[cfg(feature = "local")]
pub mod sqlite;

pub use fs_publish::LocalFsPublishStorage;
pub use noop_sync::NoopArtifactSync;
#[cfg(feature = "local")]
pub use sqlite::{
    LocalPublishStorage, SqliteBuildJobSource, SqliteBuildQueue, SqliteDb, SqliteManifestStore,
    SqliteStaticReleaseStore, SqliteWalStorage,
};
