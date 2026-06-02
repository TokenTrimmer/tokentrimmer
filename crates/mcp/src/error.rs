//! MCP server errors. Map to JSON-RPC error codes per the MCP spec:
//!   -32700 parse error · -32600 invalid request · -32601 method not found
//!   -32602 invalid params · -32603 internal · -32001 unauthorized (TT extension)
use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("parse: {0}")]
    Parse(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl McpError {
    pub fn code(&self) -> i32 {
        match self {
            Self::Parse(_) => -32700,
            Self::InvalidRequest(_) => -32600,
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::Unauthorized(_) => -32001,
            Self::Internal(_) => -32603,
        }
    }
}
