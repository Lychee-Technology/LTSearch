use serde::{Deserialize, Serialize};

pub struct ModuleBoundary;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub crate_name: String,
}
