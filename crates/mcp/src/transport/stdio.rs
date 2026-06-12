//! Line-delimited JSON-RPC over stdin/stdout.
//!
//! Each request line is capped at [`MAX_LINE_BYTES`] (matching the HTTP
//! transport's 1 MiB body cap): an oversized line is drained in bounded
//! chunks — never buffered whole, never parsed — and answered with a
//! parse-class error. Without this cap a single multi-GB line would be
//! read AND parsed in full before any tool-level size gate (e.g.
//! `run_query`'s `MAX_ARGS_BYTES`) could fire.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::error::McpError;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::server::Server;

/// Cap on one request line (content bytes, excluding the newline).
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Outcome of reading one line under the cap.
enum LineRead {
    /// `buf` holds one whole line of at most [`MAX_LINE_BYTES`] bytes.
    Line,
    /// The line exceeded the cap; its remainder was drained and discarded.
    Oversized,
}

/// Read one newline-terminated line into `buf`, never retaining more than
/// `MAX_LINE_BYTES + 1` bytes of it. Returns `None` at EOF. An oversized
/// line is consumed to its end (in bounded chunks) so the NEXT line still
/// parses cleanly.
async fn next_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<LineRead>> {
    buf.clear();
    let n = (&mut *reader)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(None); // EOF
    }
    if buf.len() > MAX_LINE_BYTES && !buf.ends_with(b"\n") {
        // Oversized: drain to end-of-line in bounded chunks, keeping nothing.
        let mut scratch = Vec::with_capacity(8 * 1024);
        loop {
            scratch.clear();
            let n = (&mut *reader)
                .take(64 * 1024)
                .read_until(b'\n', &mut scratch)
                .await?;
            if n == 0 || scratch.ends_with(b"\n") {
                break;
            }
        }
        return Ok(Some(LineRead::Oversized));
    }
    Ok(Some(LineRead::Line))
}

pub async fn run(server: Server) -> Result<(), McpError> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(read) = next_line_capped(&mut reader, &mut buf)
        .await
        .map_err(|e| McpError::Internal(format!("stdin read: {e}")))?
    {
        let resp = match read {
            LineRead::Oversized => JsonRpcResponse::err(
                None,
                McpError::Parse(String::new()).code(),
                format!("request line exceeds {MAX_LINE_BYTES} bytes"),
            ),
            LineRead::Line => {
                if buf.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<JsonRpcRequest>(&buf) {
                    Ok(req) => server.dispatch(req).await,
                    Err(e) => JsonRpcResponse::err(
                        None,
                        McpError::Parse(e.to_string()).code(),
                        e.to_string(),
                    ),
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_all(input: Vec<u8>) -> Vec<(Option<Vec<u8>>, bool)> {
        // (line content if within cap, oversized?)
        let mut reader = BufReader::new(std::io::Cursor::new(input));
        let mut buf = Vec::new();
        let mut out = Vec::new();
        while let Some(read) = next_line_capped(&mut reader, &mut buf).await.unwrap() {
            match read {
                LineRead::Line => out.push((Some(buf.clone()), false)),
                LineRead::Oversized => out.push((None, true)),
            }
        }
        out
    }

    #[tokio::test]
    async fn lines_within_cap_pass_through_unchanged() {
        let out = read_all(b"{\"a\":1}\nsecond\n".to_vec()).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.as_deref(), Some(&b"{\"a\":1}\n"[..]));
        assert_eq!(out[1].0.as_deref(), Some(&b"second\n"[..]));
    }

    #[tokio::test]
    async fn final_line_without_newline_is_still_a_line() {
        let out = read_all(b"only".to_vec()).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.as_deref(), Some(&b"only"[..]));
    }

    /// A line of exactly MAX_LINE_BYTES content (+ newline) is allowed.
    #[tokio::test]
    async fn exact_cap_line_is_allowed() {
        let mut input = vec![b'x'; MAX_LINE_BYTES];
        input.push(b'\n');
        let out = read_all(input).await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].1, "exact-cap line must not be flagged oversized");
        assert_eq!(out[0].0.as_ref().unwrap().len(), MAX_LINE_BYTES + 1);
    }

    /// An oversized line is reported as such WITHOUT being buffered, and the
    /// following line still parses cleanly (the remainder was drained to the
    /// newline, not into the next request).
    #[tokio::test]
    async fn oversized_line_is_drained_not_buffered_and_next_line_survives() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 700_000];
        input.push(b'\n');
        input.extend_from_slice(b"{\"ok\":true}\n");
        let out = read_all(input).await;
        assert_eq!(out.len(), 2);
        assert!(out[0].1, "first line must be flagged oversized");
        assert_eq!(out[1].0.as_deref(), Some(&b"{\"ok\":true}\n"[..]));
    }

    /// Oversized line at EOF (no trailing newline) terminates cleanly.
    #[tokio::test]
    async fn oversized_line_at_eof_terminates() {
        let input = vec![b'x'; MAX_LINE_BYTES + 10];
        let out = read_all(input).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].1);
    }
}
