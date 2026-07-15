//! 本地（AWS-free）契约实现。这些类型只依赖 std / tokio / 文件系统，可在任何
//! profile 下编译；`local` profile 用它们构造 runtime，#108 将以 SQLite 版本
//! 替换耐久事件与版本协调部分。

pub mod fs_build_queue;
pub mod fs_publish;
pub mod fs_wal;

pub use fs_build_queue::LocalFsBuildQueue;
pub use fs_publish::LocalFsPublishStorage;
pub use fs_wal::LocalFsWalStorage;
