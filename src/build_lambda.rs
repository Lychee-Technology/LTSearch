use serde::{Deserialize, Serialize};

use crate::error::{IndexError, PublishError};
use crate::indexing::{BuildIndexResult, PublishResult};
use crate::models::IndexManifest;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BuildRequest {
    pub batch_id: String,
    pub wal_key: String,
    pub version_id: u64,
    pub embedding_dim: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BuildResponse {
    pub activated_version_id: u64,
    pub previous_version_id: Option<u64>,
    pub document_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildLambdaError {
    pub error_type: String,
    pub message: String,
}

impl From<IndexError> for BuildLambdaError {
    fn from(error: IndexError) -> Self {
        match error {
            IndexError::Validation(source) => Self {
                error_type: "build_error".into(),
                message: source.to_string(),
            },
            IndexError::Operation { message } => Self {
                error_type: "build_error".into(),
                message: IndexError::Operation { message }.to_string(),
            },
        }
    }
}

impl From<PublishError> for BuildLambdaError {
    fn from(error: PublishError) -> Self {
        match error {
            PublishError::Validation(source) => Self {
                error_type: "publish_error".into(),
                message: source.to_string(),
            },
            PublishError::Operation { message } => Self {
                error_type: "publish_error".into(),
                message: PublishError::Operation { message }.to_string(),
            },
        }
    }
}

pub fn handle_build_request<B, P>(
    build_handler: B,
    publish_handler: P,
    request: BuildRequest,
) -> Result<BuildResponse, BuildLambdaError>
where
    B: FnOnce(&BuildRequest) -> Result<BuildIndexResult, IndexError>,
    P: FnOnce(&IndexManifest) -> Result<PublishResult, PublishError>,
{
    let build_result = build_handler(&request).map_err(BuildLambdaError::from)?;
    let publish_result = publish_handler(&build_result.manifest).map_err(BuildLambdaError::from)?;
    Ok(BuildResponse {
        activated_version_id: publish_result.activated_version_id,
        previous_version_id: publish_result.previous_version_id,
        document_count: build_result.documents.len(),
    })
}
