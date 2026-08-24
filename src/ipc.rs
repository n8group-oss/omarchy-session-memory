use serde::Serialize;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub protocol_version: u32,
    pub engine_version: String,
    pub ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl StatusReport {
    pub fn new(ready: bool, message: Option<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            ready,
            message,
        }
    }
}
