use async_trait::async_trait;

use crate::error::IngestError;
use crate::error::ValidationError;
use crate::models::WalRecord;

#[async_trait]
pub trait WalStorage: Clone + Send + Sync + 'static {
    async fn append(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError>;
    async fn read(&self, key: &str) -> Result<Vec<u8>, IngestError>;
}

#[derive(Debug, Clone)]
pub struct WriteAheadLog<S> {
    storage: S,
}

impl<S> WriteAheadLog<S>
where
    S: WalStorage,
{
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    pub async fn append(&self, key: &str, record: &WalRecord) -> Result<(), IngestError> {
        record.validate()?;

        let mut line = serde_json::to_vec(record).map_err(|error| IngestError::Operation {
            message: error.to_string(),
        })?;
        line.push(b'\n');

        self.append_bytes(key, &line).await
    }

    pub async fn append_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), IngestError> {
        self.storage.append(key, bytes).await
    }

    pub async fn read(&self, key: &str) -> Result<Vec<WalRecord>, IngestError> {
        let bytes = self.storage.read(key).await?;
        let contents = std::str::from_utf8(&bytes).map_err(|error| IngestError::Operation {
            message: error.to_string(),
        })?;

        contents
            .lines()
            .map(|line| {
                let record = serde_json::from_str::<WalRecord>(line).map_err(|error| {
                    IngestError::Operation {
                        message: error.to_string(),
                    }
                })?;
                record.validate()?;
                Ok(record)
            })
            .collect()
    }
}

/// 全部 WAL 段共享的对象 key 前缀；`segment_key` 生成的 key 都落在其下，
/// 快照构建方按此前缀列举全部段。
pub const WAL_PREFIX: &str = "wal/";

pub fn segment_key(timestamp_millis: i64, segment_id: &str) -> Result<String, IngestError> {
    validate_segment_id(segment_id)?;
    let days_since_epoch = timestamp_millis.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days_since_epoch);
    Ok(format!(
        "{WAL_PREFIX}{year:04}/{month:02}/{day:02}/{segment_id}.jsonl"
    ))
}

fn validate_segment_id(segment_id: &str) -> Result<(), IngestError> {
    if segment_id.is_empty()
        || segment_id.contains('/')
        || segment_id.contains("..")
        || segment_id.contains('\\')
    {
        return Err(IngestError::Validation(ValidationError::InvalidValue {
            field: "segment_id",
        }));
    }

    Ok(())
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };

    (year as i32, month as u32, day as u32)
}
