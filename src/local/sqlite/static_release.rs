//! SQLite read side of the `static/_head` pointer: [`SqliteStaticReleaseStore`]
//! parses the `static_release_head` single-row into a [`StaticReleaseHead`],
//! folding an absent row into `Ok(None)`. Mirrors [`super::manifest::SqliteManifestStore`]'s
//! synchronous, single-row blocking style; the pointer's write side (CAS) lives
//! in [`super::head::LocalPublishStorage`], which routes `static/_head` to this
//! same table.

use rusqlite::OptionalExtension;

use super::SqliteDb;
use crate::storage::static_release_store::invalid_head;
use crate::storage::{StaticReleaseHead, StaticReleaseStore, StaticReleaseStoreError};

pub struct SqliteStaticReleaseStore {
    db: SqliteDb,
}

impl SqliteStaticReleaseStore {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }
}

impl StaticReleaseStore for SqliteStaticReleaseStore {
    fn load_active_release(&self) -> Result<Option<StaticReleaseHead>, StaticReleaseStoreError> {
        let bytes: Option<Vec<u8>> = self
            .db
            .with_conn_blocking(|conn| {
                conn.query_row(
                    "SELECT head_bytes FROM static_release_head WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .optional()
            })
            .map_err(|error| StaticReleaseStoreError::Invalid {
                message: format!("failed to read static release head: {error}"),
            })?;
        match bytes {
            Some(bytes) => StaticReleaseHead::from_json(&bytes)
                .map(Some)
                .map_err(invalid_head),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexing::PublishStorage;
    use crate::local::sqlite::head::LocalPublishStorage;
    use crate::storage::STATIC_HEAD_KEY;

    /// CAS the given head into the `static_release_head` row via the write side.
    async fn seed_release(db: &SqliteDb, root: &std::path::Path, head: &StaticReleaseHead) {
        let publish = LocalPublishStorage::new(db.clone(), root);
        assert!(publish
            .compare_and_swap(STATIC_HEAD_KEY, None, head.to_json_pretty().as_bytes())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn empty_row_folds_to_none() {
        let (db, _dir) = SqliteDb::open_temp();
        let store = SqliteStaticReleaseStore::new(db);
        assert_eq!(store.load_active_release().unwrap(), None);
    }

    #[tokio::test]
    async fn reads_back_head_written_by_cas() {
        let (db, dir) = SqliteDb::open_temp();
        let head = StaticReleaseHead::new("a".repeat(64), 1_700_000_000_000);
        seed_release(&db, dir.path(), &head).await;

        let store = SqliteStaticReleaseStore::new(db);
        assert_eq!(store.load_active_release().unwrap(), Some(head));
    }

    #[tokio::test]
    async fn corrupt_row_is_invalid() {
        let (db, dir) = SqliteDb::open_temp();
        // Route a non-parsing byte blob into the pointer row via the write side.
        let publish = LocalPublishStorage::new(db.clone(), dir.path());
        assert!(publish
            .compare_and_swap(STATIC_HEAD_KEY, None, b"{ not valid json")
            .await
            .unwrap());

        let store = SqliteStaticReleaseStore::new(db);
        assert!(matches!(
            store.load_active_release(),
            Err(StaticReleaseStoreError::Invalid { .. })
        ));
    }
}
