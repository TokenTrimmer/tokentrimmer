# Design Spec — Phase 5 Query-Offload: `run_query` MCP tool (resident-vs-referenced type gate)

Repo: `tokentrimmer/tokentrimmer` (public). Lane: `crates/mcp` (+ CLI boot
wiring, + 2 new workspace deps). Gateway dispatch path NOT touched.

## 1. Problem and savings case

A model asked to analyze data pays input tokens for every row it reads. If
the data stays EXTERNAL (a file or DB the tool queries) and only the
COMPUTED RESULT enters context, the data token class is deleted (Anthropic
CSV example: input 105 / output 239; "$62 pasted → ~$0.002 offloaded"). The
trap is INLINE-OFFLOAD — running compute over data already resident in the
prompt — which is ~2.3× WORSE than doing nothing, because you pay the rows
AND the tool round-trip. Therefore the gate against inline data must be
**type-level**: the tool's input types must make inline data
unrepresentable, not merely discouraged in the description.

MVP scope decision: **full code execution is unjustifiable in this lane.**
The tt MCP server runs on an operator's machine; an arbitrary code/shell
primitive reachable from model-controlled tool calls (which may carry
prompt-injected instructions) is an unacceptable escalation, and sandboxing
it properly is a project, not a feature. The savings case holds with
**query-offload only**: SQL against operator-configured Postgres + a bounded
aggregation surface over CSV/JSONL files. This is recorded in module docs as
an explicit non-goal, with a pointer that a hosted execution variant would
additionally be **not ZDR-eligible** (see §4).

## 2. SECURITY MODEL (normative — the core of this spec)

### 2.1 Threat model

The MCP caller is an LLM agent; its tool arguments are
attacker-influencable (prompt injection). The operator is trusted for
boot-time config (same trust level as `DATABASE_URL` today). The tool must
not become:

- (a) an **exfil/SSRF primitive** (reading arbitrary files, hitting metadata
  endpoints, dialing internal hosts),
- (b) a **write/code-exec primitive** on the operator machine or their DB,
- (c) an **inline-offload trap** (data smuggled through parameters),
- (d) a **bulk-export channel** (raw rows flooding back into context).

### 2.2 Alias indirection: caller-supplied strings never become I/O targets

Datasets are registered at boot from an operator TOML (§5). The model can
only **name** a dataset by alias. There is no tool parameter that is a path,
URL, or DSN. Consequences:

- Path traversal is unrepresentable: file paths exist only in config, are
  required to be **relative to an operator-allowlisted root** (`files.root`,
  REQUIRED, **no default**), are canonicalized at boot AND re-canonicalized
  at every call with a containment check (symlink-swap defense; the residual
  open-vs-check TOCTOU on platforms without O_NOFOLLOW-dir semantics is
  documented as accepted residual risk).
- SSRF is unrepresentable from the tool boundary: the only network handle
  kind is Postgres via an operator DSN read from an env var (`dsn_env` —
  credentials never live in the TOML).
  `tt_shared::url_guard::validate_provider_url` is **not invoked in the MVP
  because no caller-reachable URL exists**; the spec REQUIRES it (with
  `allow_local=false`) for any future handle kind that accepts a URL (e.g.
  `kind = "https"`), recorded as a follow-up constraint in module docs.

### 2.3 Resident-vs-referenced gate — demonstrated in the type signatures

```rust
// crates/mcp/src/query/handle.rs

/// Caller-suppliable name of an operator-registered dataset.
/// INVARIANT: `[a-z0-9_-]{1,64}` — enforced in the only constructor.
pub struct HandleAlias(String);            // TryFrom<&str>; Deserialize delegates to TryFrom

/// Opaque reference to EXTERNAL (referenced, never resident) data.
///
/// TYPE-LEVEL GATE: `DatasetHandle`
///   * does NOT implement `serde::Deserialize` (pinned by a compile_fail doctest),
///   * has all-private fields and NO public constructor,
///   * is obtainable ONLY via `HandleRegistry::resolve`, whose inputs come from
///     operator boot config — never from tool arguments.
/// Caller JSON can therefore only *select* data by alias; it can never *carry*
/// data into the executor. Inline-offload is unrepresentable at this boundary.
pub struct DatasetHandle { alias: HandleAlias, /* description, */ source: DataSource }

enum DataSource {                                          // private
    File { root: PathBuf, path: PathBuf, format: FileFormat }, // canonical, under root
    Postgres { pool: sqlx::PgPool },                       // built from dsn_env at boot
}

pub struct HandleRegistry { /* HashMap<HandleAlias, DatasetHandle>, built at boot */ }
impl HandleRegistry {
    /// The ONLY way to obtain a `DatasetHandle`.
    pub fn resolve(&self, alias: &HandleAlias) -> Option<&DatasetHandle>;
}
```

```rust
// crates/mcp/src/query/bounded.rs

pub const MAX_QUERY_BYTES: usize = 4096;   // a useful CSV cannot fit
pub const MAX_ARGS_BYTES:  usize = 8192;   // outer gate on the whole arguments object

/// Size-capped query text. INVARIANT: len <= MAX_QUERY_BYTES, enforced in the
/// only constructor (TryFrom<&str>); Deserialize delegates to it. The cap is
/// the anti-smuggling gate: the parameter is useless as a data channel.
pub struct BoundedQuery(String);

/// Size-capped string for AggregationSpec fields (column names, predicate values).
pub struct BoundedStr<const N: usize>(String);
```

```rust
// crates/mcp/src/query/spec.rs — what a caller CAN express, fully bounded

pub struct RunQueryArgs {
    pub dataset: HandleAlias,     // selects by NAME only
    /* query XOR aggregate (enforced in into_parts) */
    pub verify: bool,
}

pub enum QuerySpec {
    /// Postgres handles: SQL text, capped BEFORE parsing, then sqlparser-gated (§2.4).
    Sql(BoundedQuery),
    /// File handles: structured aggregation — NOT free-form SQL.
    Aggregate(AggregationSpec),
}

pub struct AggregationSpec {
    pub op: AggOp,                                    // Count|Sum|Avg|Min|Max|CountDistinct
    pub column: Option<BoundedStr<128>>,
    pub predicates: Vec<Predicate>,                   // JSON "where"; len <= 8, enforced in Deserialize
    pub group_by: Vec<BoundedStr<128>>,               // len <= 4
    pub limit: Option<u32>,
}
pub struct Predicate { pub column: BoundedStr<128>, pub op: CmpOp, pub value: BoundedStr<256> }
```

```rust
// crates/mcp/src/query/mod.rs — the executor seam. Note what is ABSENT:
// no parameter or variant carries caller data bytes. There is no path from
// tool params to resident data.
pub(crate) async fn execute(
    handle: &DatasetHandle,        // referenced: resolved from operator config
    query:  &ValidatedQuery,       // bounded + parser-validated (§2.4)
    limits: &QueryLimits,
) -> Result<QueryOutcome, McpError>;
```

`RunQueryTool::call` enforces, in order: (1)
`serde_json::to_string(&params).len() <= MAX_ARGS_BYTES` → else
InvalidParams, zero I/O; (2) typed deserialization into `RunQueryArgs` (all
field caps fire here); (3) `registry.resolve` (unknown alias →
InvalidParams, never echoing registry internals); (4) kind/spec match (SQL
on a file handle or Aggregate on a pg handle → InvalidParams). The
compile_fail doctest pins the gate:

```rust
/// ```compile_fail
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<tt_mcp::query::DatasetHandle>();
/// ```
```

### 2.4 Postgres execution hardening (layered, honest about the real boundary)

1. **Parser allowlist** (new workspace dep `sqlparser`, Apache-2.0, no
   rand): parse with `PostgreSqlDialect`; require **exactly one** statement;
   require `Statement::Query` (SELECT / WITH..SELECT); reject everything
   else (INSERT/UPDATE/DELETE/DDL/COPY/SET/CALL/EXPLAIN). Walk the AST
   (incl. CTEs/subqueries/function args) and reject a denylist of escape
   functions: `dblink*`, `pg_read_file`, `pg_read_binary_file`,
   `pg_ls_dir`, `lo_import`, `lo_export`. Output type: `ValidatedSql`
   (private constructor — only the validator produces one; the executor only
   accepts validated SQL, so unvalidated SQL is unrepresentable at the
   execution seam). `SELECT INTO` is additionally rejected by the visitor.
2. **Read-only transaction**: `BEGIN` → `SET TRANSACTION READ ONLY` → `SET
   LOCAL statement_timeout = <limits.statement_timeout_ms>` → stream the
   query → **always ROLLBACK** (never commit). READ ONLY blocks
   writes/`nextval` even if a parser gap is found.
3. **Result caps while streaming**: fetch at most `max_result_rows + 1`
   rows; running serialized-byte counter against `max_result_bytes`;
   exceeding either → hard `result_too_large` InvalidParams telling the
   model to aggregate further (NO truncation — silent truncation is a
   wrong-number machine and re-opens bulk export into context).
4. **Documented real boundary**: the SQL surface is exactly as safe as the
   role behind `dsn_env`. Docs REQUIRE recommending a dedicated read-only
   role (`default_transaction_read_only=on`, minimal grants, no extension
   grants). Layers 1–3 are defense in depth, not a substitute.

### 2.5 File execution hardening

Streaming executors (csv crate / line-wise serde_json for JSONL): O(1)
memory, never materialize the file; per-call canonicalize + root check +
regular-file check; row/byte result caps identical to §2.4(3);
`rows_scanned` = rows read (exact, since scans are full passes). Malformed
rows are counted and surfaced as `skipped_rows` in the result envelope
(never silently dropped).

### 2.6 Registration gating (decision + justification)

**No new boolean flag. The operator config file IS the gate**:
`tt mcp --query-config <path>` (or `TT_MCP_QUERY_CONFIG`). Rationale:
`--allow-write` (#149/#153) gates tools whose backends are ambient
(DATABASE_URL, base URL) and could otherwise be enabled by accident; query
handles have NO ambient source — authoring a TOML that names datasets is
already the explicit, auditable operator act, and a `--allow-query` boolean
on top would be redundant state that can disagree with it. Semantics copy
the #149 precedent exactly: no config → `run_query`/`list_datasets`/the
query-ledger resource are **never registered**, so `tools/list` omits them
and `tools/call` returns `MethodNotFound` (-32601), NOT `Unauthorized`. A
supplied-but-unreadable/invalid config **fails boot** (`anyhow::bail!`),
mirroring the #153 fail-closed posture. Registration goes through
`Server::register_query_tools(registry, cache, ledger, limits)` for parity
with `register_write_tools`; it cannot be called with a non-empty registry
except from loaded config. The standard dispatch-level `Authenticator` check
(server.rs) applies unchanged; org binding is irrelevant (local data, no
tenancy), so query tools do NOT require DATABASE_URL.

## 3. Tool surfaces

**`run_query`** — input schema (advisory; the TYPES in §2.3 are the
enforcement, and the description says so): `{ dataset: string(<=64), query:
string(<=4096) XOR aggregate: object, verify?: boolean,
additionalProperties: false }`. Description states: accepts ONLY
operator-registered dataset aliases; never paths/URLs/DSNs/inline data;
oversized arguments are rejected; results are computed aggregates capped at
N rows. Output envelope:

```json
{ "dataset": "orders", "columns": ["region", "sum_amount"],
  "rows": [["emea", 1023.50]],
  "row_count": 3, "skipped_rows": 0,
  "verified": null,
  "runs": [{"result_hash": "blake3:.."}, {"result_hash": "blake3:.."}],
  "warning": "results differed between paired executions; query may be non-deterministic or data changed",
  "execution": { "wall_ms": 12, "rows_scanned": 105000,
                 "cache": "miss", "unit": "execution", "priced_as_tokens": false } }
```

(`verified`/`runs`/`warning` only in verify mode / on mismatch;
`rows_scanned` is `null` for Postgres.)

**`list_datasets`** — no params; returns per dataset: alias, kind
(`file|postgres`), format, operator `description`, and (CSV only, cheap)
header column names. MUST NOT return paths, roots, DSN env names, or DSNs —
asserted by test.

**Resource `mcp://tokentrimmer/query-ledger/recent`** —
`QueryLedgerResource` mirroring `CostLedgerResource`
(resources/cost_ledger.rs): last N lines of the query ledger, mime
`application/x-ndjson`.

## 4. Redaction / data-residency (documented, normative)

Module + usage docs MUST state: execution is **local by design** — dataset
rows never leave the operator's machine; only the computed result enters
model context, and *that* result is then sent to the model provider like any
other tool result. This is exactly why the MVP is local-only: a
hosted/provider-side code-execution variant would route raw data through
provider infrastructure and is **not ZDR-eligible**, recorded as a hard
constraint on any future hosted variant. Operators must size
`max_result_rows/bytes` with the understanding that aggregates DO leave the
machine.

## 5. Operator config (TOML)

```toml
[limits]                       # optional; defaults shown; hard ceilings enforced at load
statement_timeout_ms = 5000    # ceiling 60000
max_result_rows = 100          # ceiling 10000
max_result_bytes = 65536       # ceiling 1048576

[files]
root = "/abs/data/dir"         # REQUIRED if any file dataset exists; no default

[[dataset]]
alias = "orders"               # path RELATIVE to root; absolute or `..` → boot error
kind = "file"
path = "orders.csv"
format = "csv"
description = "2025 orders export"

[[dataset]]
alias = "warehouse"
kind = "postgres"
dsn_env = "TT_QUERY_DSN_WAREHOUSE"  # DSN via env only — never inline (TOML may be committed)
```

Load-time validation (all fail boot): duplicate alias; bad alias charset;
absolute/`..` path; missing root; unset `dsn_env`; limits above ceilings.
Pools are built at boot (connect failure → boot error, fail closed).

## 6. Verification (paired re-execution)

`verify: true` → two fully independent executions (fresh transaction / fresh
file read; cache **bypassed** for both), structural comparison of result
rows. Match → `verified: true` (and the cache entry is refreshed for file
handles). Mismatch → `verified: false`, BOTH runs' blake3 result hashes in
`runs`, `warning` set, first run's result returned — the mismatch is
surfaced, never swallowed; this is the only honest catch for the
silent-wrong-number failure. Docs note that nondeterministic SQL (`now()`,
`random()`, un-ORDERed LIMIT) legitimately mismatches. Both runs are metered
as ONE ledger line with `wall_ms` summed and `verify: true` — keeps
one-call == one-line.

## 7. Execution ledger + result cache

**Ledger**: append-only JSONL at `TT_MCP_QUERY_LEDGER_PATH` (default
`.claude/query-ledger.jsonl`, the cost-ledger convention). Line: `{ts,
tool:"run_query", dataset, kind, wall_ms, rows_scanned (files: exact; pg:
null in MVP — no fabricated numbers; pg_stat_statements follow-up),
result_rows, result_bytes, cache:"hit|miss|bypass", verify:bool,
verified:bool|null, unit:"execution", priced_as_tokens:false}`. Execution
cost is a DISTINCT NON-TOKEN class: nothing in this lane converts it to USD
or token counts. Write failures: `warn!` + call still succeeds.

**Cache**: in-process `QueryCache` (Mutex + simple LRU, cap 256), key =
`(blake3(file content), canonical serde serialization of the typed
AggregationSpec)`. **File handles only** — exact-by-construction. **Postgres
results are never cached** in the MVP: with no honest content fingerprint, a
cache is a stale-wrong-number machine; deferred with a design note. Cache
hit → ledger line with `cache:"hit"` and hash-only `wall_ms`.

## 8. Boot wiring (crates/cli/src/main.rs, `Command::Mcp`)

Add `#[arg(long, env = "TT_MCP_QUERY_CONFIG")] query_config:
Option<PathBuf>`. After the existing tool registrations: if present →
`tt_mcp::query::QueryConfig::load(path)` (boot error on any failure) →
`build_registry()` (connects pg pools) → `server.register_query_tools(...)`
(which also registers `QueryLedgerResource`). If absent → nothing registered
(tools/list omits; -32601 on call).

## 9. Follow-ups (recorded, NOT in this lane)

- Gateway `RouteAction` to detect tabular blobs in prompts and
  suggest/route to query-offload — owned by the chat.rs/routing lanes,
  noted only.
- Postgres content fingerprint for caching; `rows_scanned` for pg via
  pg_stat_statements/EXPLAIN.
- URL handle kind (`https` datasets) — MUST go through
  `tt_shared::url_guard::validate_provider_url(raw, false)`.
- Hosted execution variant — blocked on ZDR analysis per §4.

## 10. Delivery constraints

Conventional commits; cargo fmt --check, clippy --workspace --all-targets
-- -D warnings, cargo test --workspace (plus --no-run for ripple check) all
green; new deps `csv` + `sqlparser` only (no rand graph changes); regenerate
THIRD-PARTY-LICENSES + cargo deny pass; no snapshot drift; gateway dispatch
path untouched. Pg integration tests are `#[ignore]` + `TEST_DATABASE_URL`
per repo convention; all security gates are unit-testable without a DB.
