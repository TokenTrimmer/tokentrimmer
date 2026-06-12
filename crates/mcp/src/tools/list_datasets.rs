//! `list_datasets` — enumerate operator-registered dataset aliases.
//!
//! Returns alias, kind, format, operator description, and (CSV only, cheap)
//! header column names. MUST NOT return paths, roots, DSN env-var names, or
//! DSNs — the response goes straight into model context, and operator
//! filesystem/credential layout is none of the model's business (asserted
//! by the dispatch tests).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::query::handle::DataSource;
use crate::query::{file_exec, FileFormat, HandleRegistry};
use crate::tools::Tool;

pub struct ListDatasetsTool {
    pub registry: Arc<HandleRegistry>,
}

/// Cheap header sniff for CSV datasets: first record only, through the same
/// canonicalize+containment gate as execution. Failures are non-fatal — the
/// listing simply omits `columns`.
fn csv_headers(root: &std::path::Path, path: &std::path::Path) -> Option<Vec<String>> {
    let canonical = file_exec::checked_canonical(root, path).ok()?;
    let mut rdr = csv::Reader::from_path(canonical).ok()?;
    let headers = rdr.headers().ok()?;
    Some(headers.iter().map(str::to_owned).collect())
}

#[async_trait]
impl Tool for ListDatasetsTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "list_datasets",
            description: "List the operator-registered datasets available to run_query: alias, kind (file|postgres), format, description, and CSV column names. Datasets are registered by the operator at boot — they cannot be added, read directly, or addressed by path/URL from here.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, _params: Value) -> Result<Value, McpError> {
        let datasets: Vec<Value> = self
            .registry
            .iter_sorted()
            .into_iter()
            .map(|h| {
                let mut entry = json!({
                    "alias": h.alias().as_str(),
                    "kind": h.kind_str(),
                });
                if let Some(format) = h.file_format() {
                    entry["format"] = json!(format.as_str());
                }
                if let Some(desc) = h.description() {
                    entry["description"] = json!(desc);
                }
                if let DataSource::File {
                    root,
                    path,
                    format: FileFormat::Csv,
                } = h.source()
                {
                    if let Some(cols) = csv_headers(root, path) {
                        entry["columns"] = json!(cols);
                    }
                }
                entry
            })
            .collect();
        Ok(json!({ "datasets": datasets }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryConfig;

    #[tokio::test]
    async fn lists_aliases_and_csv_columns_without_leaking_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orders.csv"), "region,amount\nemea,1\n").unwrap();
        let toml = format!(
            r#"
            [files]
            root = "{}"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "orders.csv"
            format = "csv"
            description = "2025 orders export"
            "#,
            dir.path().display()
        );
        let cfg = QueryConfig::from_toml_str(&toml, &|_| None).unwrap();
        let registry = Arc::new(cfg.build_registry().await.unwrap());
        let t = ListDatasetsTool { registry };

        let v = t.call(json!({})).await.unwrap();
        let ds = &v["datasets"][0];
        assert_eq!(ds["alias"], "orders");
        assert_eq!(ds["kind"], "file");
        assert_eq!(ds["format"], "csv");
        assert_eq!(ds["description"], "2025 orders export");
        assert_eq!(ds["columns"], json!(["region", "amount"]));

        let serialized = serde_json::to_string(&v).unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(
            !serialized.contains(root.to_str().unwrap()),
            "response leaked the root: {serialized}"
        );
        assert!(
            !serialized.contains("orders.csv"),
            "response leaked the file path: {serialized}"
        );
    }
}
