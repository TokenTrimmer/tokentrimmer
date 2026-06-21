//! `tt batch` — the first-party consumer surface for the async Batch Lane.
//! Submit a JSONL request file, list/inspect/cancel batches, and download the
//! result JSONL once a batch completes. Everything goes through the gateway's
//! OpenAI-compatible Batch endpoints via the tested [`tt_client`] methods.
//!
//! The Batch Lane is for latency-INSENSITIVE traffic: the gateway returns a
//! handle immediately and the cloud poll worker drives the batch to completion
//! over its window (≤24h), so these commands never block on the model — `submit`
//! hands back an id, and `get`/`list` report progress against it.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Subcommand;

use crate::context::ResolvedContext;
use crate::ui;

/// `tt batch <action>` subcommands.
#[derive(Subcommand)]
pub enum BatchAction {
    /// Submit a JSONL request file as a new batch (uploads it, then creates the
    /// batch and prints the new id + status).
    Submit {
        /// Path to the JSONL file (one request object per line).
        file: PathBuf,
        /// The endpoint the batch targets.
        #[arg(long, default_value = "/v1/chat/completions")]
        endpoint: String,
        /// The completion window.
        #[arg(long, default_value = "24h")]
        window: String,
    },
    /// List your batches (newest-first) as a compact table.
    List,
    /// Show one batch by id (status, counts, file ids).
    Get {
        /// The batch id (e.g. `batch_abc123`).
        id: String,
    },
    /// Cancel a batch by id and print its resulting status.
    Cancel {
        /// The batch id.
        id: String,
    },
    /// Download a file's content (the result/error JSONL of a completed batch).
    Download {
        /// The file id (e.g. a batch's `output_file_id`).
        id: String,
        /// Write the bytes here instead of stdout.
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
}

/// Options for [`run`], carrying the global key/base overrides.
pub struct RunOpts {
    pub action: BatchAction,
    pub flag_key: Option<String>,
    pub flag_base: Option<String>,
}

/// `tt batch` entry point: resolve the context, build a client, and dispatch.
pub async fn run(opts: RunOpts) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(opts.flag_key, opts.flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    match opts.action {
        BatchAction::Submit {
            file,
            endpoint,
            window,
        } => submit(&client, &file, &endpoint, &window).await,
        BatchAction::List => list(&client).await,
        BatchAction::Get { id } => get(&client, &id).await,
        BatchAction::Cancel { id } => cancel(&client, &id).await,
        BatchAction::Download { id, output } => download(&client, &id, output.as_deref()).await,
    }
}

/// Upload the JSONL file, create the batch, and report the new handle.
async fn submit(
    client: &tt_client::Client,
    file: &std::path::Path,
    endpoint: &str,
    window: &str,
) -> anyhow::Result<()> {
    let jsonl = std::fs::read(file)
        .with_context(|| format!("reading batch input file {}", file.display()))?;
    ui::heading("Submitting batch");
    let file_id = client
        .upload_batch_input(jsonl)
        .await
        .map_err(|e| anyhow::anyhow!("uploading batch input: {e}"))?;
    ui::note(&format!("uploaded input file: {file_id}"));
    let batch = client
        .create_batch(&file_id, endpoint, window)
        .await
        .map_err(|e| anyhow::anyhow!("creating batch: {e}"))?;
    ui::ok(&format!(
        "batch {} created · status {}",
        batch.id, batch.status
    ));
    // The id is the result — to stdout so it can be piped into `tt batch get`.
    println!("{}", batch.id);
    Ok(())
}

/// List batches as a compact table.
async fn list(client: &tt_client::Client) -> anyhow::Result<()> {
    let batches = client
        .list_batches()
        .await
        .map_err(|e| anyhow::anyhow!("listing batches: {e}"))?;
    if batches.is_empty() {
        ui::note("no batches yet — submit one with `tt batch submit <file.jsonl>`");
        return Ok(());
    }
    let mut table = ui::table(
        &["ID", "STATUS", "COMPLETED/TOTAL", "FAILED", "CREATED"],
        console::colors_enabled(),
    );
    for b in &batches {
        table.add_row(vec![
            b.id.clone(),
            b.status.clone(),
            format!("{}/{}", b.request_counts.completed, b.request_counts.total),
            b.request_counts.failed.to_string(),
            b.created_at
                .map_or_else(|| "-".to_string(), |t| t.to_string()),
        ]);
    }
    println!("{table}");
    ui::note(&format!("{} batches", batches.len()));
    Ok(())
}

/// Show a single batch in detail.
async fn get(client: &tt_client::Client, id: &str) -> anyhow::Result<()> {
    let b = client
        .get_batch(id)
        .await
        .map_err(|e| anyhow::anyhow!("fetching batch {id}: {e}"))?;
    ui::heading(&format!("Batch {}", b.id));
    ui::note(&format!("status: {}", b.status));
    ui::note(&format!(
        "requests: {} completed · {} failed · {} total",
        b.request_counts.completed, b.request_counts.failed, b.request_counts.total
    ));
    if let Some(ep) = &b.endpoint {
        ui::note(&format!("endpoint: {ep}"));
    }
    if let Some(input) = &b.input_file_id {
        ui::note(&format!("input file: {input}"));
    }
    if let Some(out) = &b.output_file_id {
        ui::note(&format!("output file: {out}"));
    }
    if let Some(err) = &b.error_file_id {
        ui::note(&format!("error file: {err}"));
    }
    // When completed, point at the next step.
    if b.status == "completed" {
        if let Some(out) = &b.output_file_id {
            ui::ok(&format!("results ready — `tt batch download {out}`"));
        }
    }
    Ok(())
}

/// Cancel a batch and report the new status.
async fn cancel(client: &tt_client::Client, id: &str) -> anyhow::Result<()> {
    let b = client
        .cancel_batch(id)
        .await
        .map_err(|e| anyhow::anyhow!("cancelling batch {id}: {e}"))?;
    ui::ok(&format!("batch {} · status {}", b.id, b.status));
    Ok(())
}

/// Download a file's bytes to `output` (or stdout).
async fn download(
    client: &tt_client::Client,
    file_id: &str,
    output: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let bytes = client
        .download_file_content(file_id)
        .await
        .map_err(|e| anyhow::anyhow!("downloading file {file_id}: {e}"))?;
    match output {
        Some(path) => {
            std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            ui::ok(&format!(
                "wrote {} bytes to {}",
                bytes.len(),
                path.display()
            ));
        }
        None => {
            use std::io::Write as _;
            std::io::stdout()
                .write_all(&bytes)
                .context("writing batch output to stdout")?;
        }
    }
    Ok(())
}
