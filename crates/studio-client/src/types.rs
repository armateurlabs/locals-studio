use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn from_result(result: anyhow::Result<Value>) -> Self {
        match result {
            Ok(value) => Self {
                result: Some(value),
                error: None,
            },
            Err(error) => Self {
                result: None,
                error: Some(format!("{error:#}")),
            },
        }
    }
}
