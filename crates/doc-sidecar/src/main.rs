//! Binary entry point for the Document Lane OCR/parse sidecar.
//!
//! Binds an axum server exposing [`doc_sidecar::app`]. The listen address comes
//! from `DOC_SIDECAR_ADDR` (default `127.0.0.1:8088`). Run with `--features ocr`
//! and `TT_OCR_DETECTION_MODEL` / `TT_OCR_RECOGNITION_MODEL` to enable image OCR.

use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let addr: SocketAddr = std::env::var("DOC_SIDECAR_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8088".to_string())
        .parse()?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "doc-sidecar listening");
    axum::serve(listener, doc_sidecar::app()).await?;
    Ok(())
}
