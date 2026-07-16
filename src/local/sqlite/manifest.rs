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

    #[tokio::test]
    async fn load_head_reads_active_head_row() {
        let (db, _dir) = SqliteDb::open_temp();
        let publish = LocalPublishStorage::new(db.clone(), _dir.path());
        let head = ManifestHead::new(7, 1_700_000_000_000);
        assert!(publish
            .compare_and_swap(INDEX_HEAD_KEY, None, &head.to_json_pretty())
            .await
            .unwrap());

        let store = SqliteManifestStore::new(db, _dir.path());
        let loaded = store.load_head().unwrap();

        assert_eq!(loaded, head);
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
}
