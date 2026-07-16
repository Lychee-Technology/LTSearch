//! SQLite 版活跃版本读取：`ManifestStore::load_head` 从 `active_head` 行解析
//! `ManifestHead`；`load_active_manifest` 再按 head 里的 `manifest_path` 读盘上的
//! manifest 文件（制品仍在共享卷）。与 `LocalManifestStore` 的文件版语义一致，只是
//! head 的真源从 `index/_head` 文件换成 SQLite 单行，和发布侧的 head-CAS 对齐。

use std::path::PathBuf;

use rusqlite::OptionalExtension;

use super::SqliteDb;
use crate::models::IndexManifest;
use crate::storage::{
    ActiveManifest, HeadError, ManifestHead, ManifestStore, ManifestStoreError, INDEX_HEAD_KEY,
};

pub struct SqliteManifestStore {
    db: SqliteDb,
    root: PathBuf,
}

impl SqliteManifestStore {
    pub fn new(db: SqliteDb, root: impl Into<PathBuf>) -> Self {
        Self {
            db,
            root: root.into(),
        }
    }
}

impl ManifestStore for SqliteManifestStore {
    fn load_head(&self) -> Result<ManifestHead, ManifestStoreError> {
        let bytes: Option<Vec<u8>> = self
            .db
            .with_conn_blocking(|conn| {
                conn.query_row(
                    "SELECT head_bytes FROM active_head WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()
            })
            .map_err(|error| ManifestStoreError::InvalidHead {
                message: format!("failed to read active head: {error}"),
            })?;
        let bytes = bytes.ok_or_else(|| ManifestStoreError::MissingHead {
            path: PathBuf::from(INDEX_HEAD_KEY),
        })?;
        ManifestHead::from_json(&bytes).map_err(|error| ManifestStoreError::InvalidHead {
            message: match error {
                HeadError::Parse { message } => message,
                other => other.to_string(),
            },
        })
    }

    fn load_active_manifest(&self) -> Result<ActiveManifest, ManifestStoreError> {
        let head = self.load_head()?;
        let path = self.root.join(&head.manifest_path);
        let contents = std::fs::read_to_string(&path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => {
                ManifestStoreError::MissingManifest { path: path.clone() }
            }
            _ => ManifestStoreError::Io {
                path: path.clone(),
                source,
            },
        })?;
        let manifest: IndexManifest = serde_json::from_str(&contents).map_err(|source| {
            ManifestStoreError::InvalidManifest {
                path: path.clone(),
                message: source.to_string(),
            }
        })?;
        manifest
            .validate()
            .map_err(|source| ManifestStoreError::InvalidManifest {
                path: path.clone(),
                message: source.to_string(),
            })?;
        if manifest.version_id != head.version_id {
            return Err(ManifestStoreError::InvalidManifest {
                path,
                message: "manifest version_id must match _head version_id".into(),
            });
        }
        Ok(ActiveManifest { head, manifest })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::PublishStorage;
    use crate::local::sqlite::head::LocalPublishStorage;
    use crate::storage::version_manifest_key;

    fn sample_manifest_json(version_id: u64) -> String {
        format!(
            r#"{{
  "version_id": {version_id},
  "created_at": 1700000000000,
  "embedding_dim": 768,
  "document_count": 5,
  "num_shards": 2,
  "shards": [
    {{ "shard_id": 0, "document_count": 2,
       "lance_path": "s3://bucket/lance/v{version_id}/shard_0",
       "tantivy_path": "s3://bucket/index/v{version_id}/shard_0" }},
    {{ "shard_id": 1, "document_count": 3,
       "lance_path": "s3://bucket/lance/v{version_id}/shard_1",
       "tantivy_path": "s3://bucket/index/v{version_id}/shard_1" }}
  ]
}}"#
        )
    }

    /// 把 head 写入 active_head 行（经发布侧 CAS）。
    async fn seed_head(db: &SqliteDb, root: &std::path::Path, version_id: u64) {
        let publish = LocalPublishStorage::new(db.clone(), root);
        let head = ManifestHead::new(version_id, 1_700_000_000_000);
        assert!(publish
            .compare_and_swap(INDEX_HEAD_KEY, None, &head.to_json_pretty())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn load_head_reads_active_head_row() {
        let (db, dir) = SqliteDb::open_temp();
        seed_head(&db, dir.path(), 7).await;

        let store = SqliteManifestStore::new(db, dir.path());
        let loaded = store.load_head().unwrap();

        assert_eq!(loaded, ManifestHead::new(7, 1_700_000_000_000));
    }

    #[tokio::test]
    async fn load_head_missing_is_error() {
        let (db, dir) = SqliteDb::open_temp();
        let store = SqliteManifestStore::new(db, dir.path());
        assert!(matches!(
            store.load_head(),
            Err(ManifestStoreError::MissingHead { .. })
        ));
    }

    #[tokio::test]
    async fn load_active_manifest_reads_parses_validates_and_version_checks() {
        let (db, dir) = SqliteDb::open_temp();
        seed_head(&db, dir.path(), 7).await;
        // manifest 文件落在 head.manifest_path 指向的盘上位置。
        let manifest_path = dir.path().join(version_manifest_key(7));
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, sample_manifest_json(7)).unwrap();

        let store = SqliteManifestStore::new(db, dir.path());
        let active = store.load_active_manifest().unwrap();

        assert_eq!(active.head, ManifestHead::new(7, 1_700_000_000_000));
        assert_eq!(active.manifest.version_id, 7);
        assert_eq!(active.manifest.num_shards, 2);
    }

    #[tokio::test]
    async fn load_active_manifest_missing_manifest_file_is_error() {
        let (db, dir) = SqliteDb::open_temp();
        seed_head(&db, dir.path(), 7).await; // head present, manifest file absent
        let store = SqliteManifestStore::new(db, dir.path());
        assert!(matches!(
            store.load_active_manifest(),
            Err(ManifestStoreError::MissingManifest { .. })
        ));
    }

    #[tokio::test]
    async fn load_active_manifest_version_mismatch_is_error() {
        let (db, dir) = SqliteDb::open_temp();
        seed_head(&db, dir.path(), 7).await;
        // head 指向 v7，但盘上写了 v8 的 manifest → 版本校验必须失败。
        let manifest_path = dir.path().join(version_manifest_key(7));
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, sample_manifest_json(8)).unwrap();

        let store = SqliteManifestStore::new(db, dir.path());
        assert!(matches!(
            store.load_active_manifest(),
            Err(ManifestStoreError::InvalidManifest { .. })
        ));
    }
}
