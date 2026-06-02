//! Line-delimited JSON-RPC over stdin/stdout.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::error::McpError;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::server::Server;

pub async fn run(server: Server) -> Result<(), McpError> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|e| McpError::Internal(format!("stdin read: {e}")))?
    {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(req) => server.dispatch(req).await,
            Err(e) => {
                JsonRpcResponse::err(None, McpError::Parse(e.to_string()).code(), e.to_string())
            }
        };
        let s = serde_json::to_string(&resp).unwrap();
        stdout
            .write_all(s.as_bytes())
            .await
            .map_err(|e| McpError::Internal(format!("stdout write: {e}")))?;
        stdout
            .write_all(b"\n")
            .await
            .map_err(|e| McpError::Internal(format!("stdout write: {e}")))?;
        stdout.flush().await.ok();
    }
    Ok(())
}
