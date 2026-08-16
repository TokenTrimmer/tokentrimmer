# Local price-first coding agent: threat model and cost contract

Status: design gate for the local beta. This document does not authorize hosted
code execution, vendor subscription use, or a security claim. Runtime work must
preserve every invariant below and attach behavioral evidence before release.

The machine-readable cost contract is
[`agent-cost-evidence.schema.json`](agent-contract/agent-cost-evidence.schema.json).
Rust owns the wire type and semantic validator in
`crates/shared/src/agent_cost.rs`; `tt-ts-types` generates the JSON Schema and
TypeScript/Python projections.

## Scope and non-goals

The first product is a **single-user local supervisor** running in a repository
that the user selected. It may call the existing authenticated
`/v1/agent/runs` API, a user-installed official vendor runtime, or a configured
customer-owned endpoint. It observes structured events, brokers tools, applies
policy, and emits a patch plus evidence.

Out of scope:

- hosted multi-tenant repository checkout or arbitrary code execution;
- custody, copying, parsing, or forwarding of Codex/Claude session tokens;
- treating a prompt, model-generated tool call, vendor rule file, or repository
  instruction as an authorization boundary;
- claiming a subscription run incurred API cash spend;
- claiming self-hosted inference is free without a versioned TCO profile;
- arbitrary remote BYOK forwarding;
- silent fallback from local/no-egress execution to an external model;
- sandbox-escape resistance without an independently reviewed OS/container
  isolation boundary.

## Security objectives

1. A model can request an action but cannot grant itself authority.
2. Organization policy cannot be weakened by repository content, task flags, or
   model output.
3. Repository instructions are untrusted data until a deterministic policy
   parser accepts them.
4. Filesystem, process, network, time, turn, retry, and diff bounds are enforced
   by the supervisor outside the model conversation.
5. The runner never reads or exports vendor authentication stores.
6. Every provider attempt receives a stable idempotency identity and is charged
   to the run, including retries, summarizers, judges, routing calls, embeddings,
   and shadow work.
7. Unknown cost remains `unmeasured`; it never becomes numeric zero.
8. A failure still emits the available patch, checks, decisions, stop reason,
   cost components, and receipt eligibility state.
9. Secrets and excluded paths never enter prompts, tool output, logs, patches,
   receipts, caches, or crash reports.
10. Local/no-egress policy fails closed before dispatch and before any external
    judge or summarizer call.

## Assets

| Asset | Required protection |
|---|---|
| Repository and uncommitted work | No writes outside allowed roots; no overwrite or rollback without policy and approval. |
| Git credentials and signing keys | Never expose to model context; invoke only an explicitly allowed brokered operation. |
| Environment, keychains, SSH material, cloud credentials | Default-deny reads and inheritance; redact before persistence. |
| Vendor login/session state | Vendor runtime owns it; TokenTrimmer may inspect only documented version/auth-status signals that contain no credential. |
| TokenTrimmer API key | Send only to the configured canonical gateway origin; never to tool subprocesses or vendor runtimes. |
| Provider/API budget | Reserve before dispatch, settle once, and stop before a turn that cannot fit. |
| Policy and approvals | Bind to exact bytes/revision; preserve issuer and precedence evidence. |
| Patch, transcript, checks, and receipts | Integrity, run identity, bounded retention, explicit completeness, and replayable provenance. |
| Local endpoint and model provenance | Record endpoint profile, engine/model revisions, and TCO profile; never infer identity from a display name. |

## Trust boundaries

```text
user / organization administrator
            |
            v
signed organization policy        trusted only after signature + scope checks
            |
            v
local TokenTrimmer supervisor      security and accounting boundary
       |          |          |
       |          |          +--> TokenTrimmer gateway (tt_live_ identity)
       |          +-------------> official vendor runtime (vendor owns auth)
       +------------------------> sandboxed filesystem/process/network brokers
                                      |
                                      v
                              untrusted repository + tools
```

The model, repository, tool output, compiler diagnostics, fetched content, and
vendor event payloads are untrusted. The local supervisor is trusted to enforce
policy, but a process running with the user's full account privileges is not a
strong filesystem or network sandbox. Product copy must call controls
“enforced” only when the selected platform backend provides the corresponding
kernel/container boundary and its acceptance probe passes.

## Adversaries and abuse cases

| Threat | Failure mode | Required control | Required evidence |
|---|---|---|---|
| Prompt injection in source/docs/issues | Model requests secret reads, egress, or policy edits | Instructions never change authority; broker checks every concrete action | Corpus showing injected instructions denied while ordinary reads continue |
| Malicious tool output | ANSI/control text or forged “approval” changes behavior | Treat output as bytes/data; strip terminal controls; approvals originate only from supervisor UI/IPC | Structured-output and terminal-escape fixtures |
| Path traversal or symlink swap | Allowed path resolves outside repository between check and use | Descriptor-relative operations, no-follow semantics, canonical root checks, and post-open identity checks | Traversal, hard-link, symlink, rename-race tests per platform |
| Shell/argument injection | Model text becomes a shell program | Execute argv through an allowlisted broker; shell use is a separate explicit capability | Metacharacter/newline/NUL corpus; exact argv audit |
| Process escape | Child forks, daemonizes, or outlives timeout | Process group/job containment, inherited-handle minimization, bounded output, kill-and-drain | Descendant, timeout, signal, and output-flood tests |
| Network exfiltration | Tool/model sends repository or secrets to an unapproved host | Network default deny, exact host/port/protocol allowlist, DNS rebinding defense, proxy-env removal | Direct IP, redirect, DNS-rebind, localhost, metadata, IPv6 tests |
| Secret discovery | `.env`, keychain, SSH, cloud metadata, or process env reaches context | Sensitive-path denylist, minimal environment, output redaction, no credential-store reads | Canary secrets absent from prompts, logs, patches, receipts, and crashes |
| Policy downgrade | Repo/task/model disables a signed restriction | Fixed precedence and monotonic intersection; weakening produces a typed denial | Property tests across every precedence level and unknown field |
| Approval replay/confusion | Approval for one command/path applies to another | Bind approval to run, policy hash, exact normalized action, expiry, and one use | Mutation, expiry, replay, and cross-run tests |
| Cost bypass | Retry/auxiliary call lacks reservation or accounting | One run ledger; stable component/dispatch ids; reserve before every API attempt | Concurrent/restart/retry tests with exact component coverage |
| False zero cost | Unknown price/quota/TCO is rendered as `$0` | Tagged `unmeasured` basis with nonempty reasons | Serialization/UI fixtures proving amount fields are absent |
| Subscription-token custody | Supervisor reads or forwards local vendor OAuth/session material | Spawn/use official runtime through documented interfaces; never inspect auth files or proxy the token | Filesystem-access trace and capture server showing no token leaves vendor path |
| Receipt tampering | Patch/check/cost evidence is changed after run | Hash exact artifacts and canonical evidence; sign only complete eligible records | Golden replay vectors and single-byte mutation failures |
| Resource exhaustion | Huge repo, tool output, diff, model loop, or queue consumes machine | File/count/byte/time/turn/retry/output/concurrency limits before allocation/work | Boundary and cancellation tests |
| Dependency or binary substitution | Different vendor/runtime or tool executes than reviewed | Resolve executable once; record absolute path, digest/version, and capability probe | Version/digest mismatch refusal |
| Cross-run contamination | Session, output, or approval leaks between runs | Run-scoped directories, handles, caches, ids, and cleanup | Concurrent isolation tests |

## Mandatory authorization model

The effective policy is the monotonic intersection of all applicable layers:

```text
signed organization policy
  > repository .tokentrimmer/agent.toml
    > task flags
      > model-generated request
```

A lower layer may narrow authority but never widen it. Missing or malformed
higher policy fails closed. Unknown fields fail closed. Signature failure,
issuer/scope mismatch, expiry, rollback to an older policy revision, and a
repository attempting to replace the organization policy are typed errors, not
warnings.

The policy implementation must independently govern:

- readable and writable roots, file count/size, symlinks, and sensitive paths;
- executable path plus argv patterns, subprocess count, duration, and output;
- network default, destinations, redirects, DNS results, and proxy inheritance;
- models, providers, runners, API calls, turns, retries, wall time, and diff size;
- cash/quota/TCO basis-specific budgets;
- destructive actions and one-use approvals;
- required checks, regression behavior, and rollback permissions.

### Policy wire and precedence contract

`tt init` installs a committed `.tokentrimmer/agent.toml` with
`schema_version = 1`. Its generated state is deliberately inert: repository
reads have zero byte/file allowance and runners, models, commands, network
destinations, calls, turns, writes, and cost bases are empty or zero until the
operator edits the file. Runtime state and secrets under `.tokentrimmer/` remain
ignored; only `agent.toml` is re-included.

The strict TOML maps to `tt_cli::agent_policy::AgentPolicy`. Every nested object
rejects unknown fields. Roots are canonical repository-relative paths.
`excluded_paths` are additive deny-only repository globs: absolute paths,
parent traversal, backslashes, and negated patterns are invalid. An allowed
command names one absolute executable or one separator-free executable name and
one or more exact argv prefixes; `[[]]` is the explicit “any argv” spelling.
The process broker must resolve a name once and retain the absolute path and
digest. Network destinations are exact lowercase scheme/host/port triples;
wildcards, implicit ports, redirects, and proxy inheritance are not defaults.
Destructive work and rollback have only `deny` and `one_use_approval` states;
unattended authorization is intentionally not representable in v1.

Organization policy is distributed as this strict JSON envelope:

```json
{
  "envelope_version": 1,
  "payload_base64": "<exact UTF-8 JSON payload bytes>",
  "signature_hex": "<Ed25519 signature>"
}
```

The signature covers
`\"tokentrimmer-agent-org-policy:v1\\0\" || payload_bytes`. The payload contains
`schema_version`, `issuer`, `key_id`, `organization_id`, `repository_id`,
monotonic `revision`, `issued_at`, `expires_at`, and a complete `policy`.
The issuer/key/scope expectations and 32-byte verifying key are pinned outside
the repository. The supervisor must also load the highest previously accepted
revision from durable state outside the repository and pass it as the minimum;
there is no optional rollback check. Policy files are capped at 1 MiB and
symlinked policy files are refused.

Resolution records the exact signed-payload hash, exact repository-TOML hash,
semantic task/request hashes, issuer, and revision. Allowed roots, command argv
prefixes, destinations, runners, providers, models, and cost bases can only
shrink. Numeric ceilings can only decrease. Permission booleans and approval
gates can only tighten. Sensitive-path exclusions and required validations are
ordered unions, so lower layers cannot remove them. Any attempted widening
returns a typed `PolicyWidening` error naming the layer and field rather than
silently intersecting it away. If organization policy is configured as
required, absence cannot fall back to repository policy.

### Implemented local execution broker boundary

`tt_cli::execution_broker::LocalExecutionBroker` is the mandatory
`ToolExecutor` for future local vendor adapters. It accepts only an already
resolved `ResolvedAgentPolicy`; tool descriptions are emitted only for
capabilities with nonempty authority and nonzero ceilings, and every concrete
call is reauthorized. The broker is not wired to a hosted or vendor runner by
this change.

The broker copies only approved regular files into a run-scoped capability
directory. `cap-std` relative opens, no-follow checks before and after reads,
regular-file identity checks, hard-link refusal, additive glob exclusions,
bounded directory scans, per-file/aggregate byte ceilings, and atomic staged
writes keep model-visible work away from the selected checkout. Commands get a
fresh copy of that staged workspace. Their results are accepted only when every
change remains below the writable-root, changed-file, file-size, write-byte,
and unified-diff ceilings. The source checkout is never an output path. File
deletion is not exposed in the local beta, so a model cannot invent or replay an
approval token; destructive and rollback approval gates remain unavailable
until a durable one-use token service exists.

An external command must match one policy executable and exact argv prefix.
Names resolve once; the resolved path and SHA-256 are retained. Arguments are
passed as an argv vector, never interpolated into a model-authored shell
command. Known shell executables additionally require `allow_shell = true`.
The child receives a cleared, fixed environment, null stdin, bounded output,
file-size and CPU limits, a run wall-time deadline, a process-group kill on
timeout/output breach, and a per-run external-command start ceiling. On macOS
the broker executes the hashed installed binary because copied Apple platform
binaries are not executable; replacement of that installed binary by a
same-user local adversary between hashing and exec remains a documented
residual risk. On Linux the resolved binary is staged into the namespace.

Process authority fails closed unless an operating-system backend passes an
acceptance probe at broker construction. The macOS Seatbelt probe must permit a
run-directory canary while denying the selected checkout and a loopback network
connection. The Linux backend requires a working Bubblewrap PID/network
namespace probe. The command sandbox exposes only its copied workspace/runtime
plus required system runtime paths, denies network independently of prompt
text, and terminates the command process group on every resource breach.
`max_subprocesses` currently bounds broker-started external commands; descendant
cardinality inside one authorized build command is not separately expressible
in policy v1 and remains in the independent sandbox-review gate.

`fetch_url` accepts only exact lowercase scheme/host/port destinations. It
clears proxy inheritance, rejects URL credentials and fragments, resolves once,
rejects unsafe/link-local results and hostname-to-private rebinding, pins the
validated address set into a no-redirect client, bounds connect/request/body
time and bytes, and reauthorizes every redirect when redirects are explicitly
enabled. Literal loopback/private addresses remain possible only when the
policy names that address exactly.

Every call records an action hash plus an allowed/typed-denial decision.
Process evidence includes resolved path, executable/argv hashes, status,
timeout/output-limit state, and duration; network evidence includes a URL hash,
safe origin, pinned addresses, redirects, status, and bytes. Terminal control
sequences are removed before untrusted command, file, or HTTP text is returned
to a terminal-facing model loop. `BrokerEvidence` exposes these records,
bounded read/write/output counters, and the complete staged patch on success or
failure without retaining secrets or unrestricted environment state.

### Implemented hosted run-reservation boundary

A capped `/v1/agent/runs` segment attaches one clone-shared
`RunBudgetState` to its request contexts. The hosted registry's existing
`BudgetedProvider` choke point reserves integer micro-USD against that run
ledger before each primary, retry, failover, panel, shadow, summarizer, judge,
or embedding attempt, then applies the existing durable organization/API-key
reservation. A durable rejection releases the run reservation before any
upstream call. Unknown price, duplicate dispatch identity, or insufficient
remaining cash rejects before dispatch. Success settles priced provider usage;
errors, timeouts, cancelled streams, and missing usage settle the conservative
admitted estimate.

Paused state retains the settled total and component records. Resume restores
both the remaining ceiling and prior dispatch identities; legacy paused records
seed the total from served plus summarizer cost. Detached quality and summary
judges are disabled for agent-run contexts because their spend could settle
after terminal or paused evidence was finalized. The run ledger complements,
not replaces, the durable monthly scope. A DB-less gateway that did not install
the hosted provider decorator retains only the older directional loop preview
and is not sufficient for the hosted `token_trimmer_api` runner contract.

## Runner-specific boundaries

### TokenTrimmer API runner

- Uses a `tt_live_` principal and the existing `/v1/agent/runs` contract.
- Creates one stable run id and a distinct stable component/dispatch identity for
  every provider attempt.
- Uses durable reservation and settlement for every metered turn and auxiliary
  call. A local run-level preview is not a substitute for gateway admission.
- Unknown pricing with a USD ceiling fails before dispatch.

### Official local vendor runtime

- The user authenticates directly with the vendor runtime.
- TokenTrimmer may invoke documented SDK/structured CLI surfaces and consume
  structured events. It must not read auth databases/files, environment tokens,
  browser storage, or keychains.
- Authentication status is a capability signal only. Never persist a token,
  cookie, authorization header, or raw auth diagnostic.
- Subscription use is recorded with `basis: subscription`. Marginal cash may be
  measured zero; allocation and API-equivalent values remain separate optional
  fields. Quota is unmeasured when the runtime does not expose it.
- Commercial support remains gated by applicable vendor terms and approvals.

### Customer-owned endpoint

- Uses an explicit endpoint/authentication profile and independent no-egress
  policy. Arbitrary public endpoints are not silently trusted as “local.”
- Cost uses `basis: self_hosted` only with at least one measured component from a
  versioned TCO profile. Otherwise it is `unmeasured` with an expected
  `self_hosted` basis.
- External validation/judging is forbidden under no-egress policy.

## Cost evidence contract

`AgentRunCostEvidence` v1 contains one bounded component per independently
attributable operation. Money uses signed 64-bit integer **micro-USD** values;
semantic validation rejects negatives. Component ids are unique within a run,
and retries use separate components and one-indexed attempts.

The four bases are intentionally disjoint:

| Basis | Realized fields | Separate/non-additive fields | Evidence rule |
|---|---|---|---|
| `api_metered` | `amount_micros` | none | State is `billed`, `estimated`, or `invoice_reconciled`; only the last names an invoice artifact. |
| `subscription` | `marginal_cash_micros` | `allocated_plan_micros`, `api_equivalent_micros`, quota/window | Zero marginal cash is valid; missing allocation or quota is not inferred. |
| `self_hosted` | energy, hardware amortization, hosting, operator micro-USD components | none | At least one component and a profile id/revision are required. |
| `unmeasured` | no numeric amount | expected basis plus nonempty reason list | Numeric zero is structurally absent. |

`api_equivalent_micros` is a counterfactual, not a saving and not cash. An
allocated plan amount is a user-configured accounting choice, not a marginal
charge. Self-hosted component totals must never be compared to API cash without
showing the profile, allocation window, and evidence freshness.

The Rust validator additionally enforces schema version, nonempty/bounded
components and reasons, unique component ids, nonnegative money, nonempty local
TCO, valid quota bounds, and an explanation for `other` unmeasured reasons.
JSON Schema alone cannot express every cross-field invariant; consumers must run
the semantic validator or an equivalent implementation.

## Evidence emitted on every terminal path

A successful, stopped, cancelled, policy-denied, budget-exhausted, breached, or
failed run must retain a bounded envelope containing:

- run id, runner kind/version, repository revision, and effective policy hash;
- requested and completed actions with approvals and typed denials;
- patch/artifact hashes and validation command results;
- stop reason and whether cleanup/rollback completed;
- every cost component plus measured/unmeasured coverage;
- receipt eligibility and explicit reasons when no receipt can be minted.

Raw secrets, unrestricted environment snapshots, vendor auth diagnostics, and
unbounded transcripts are never evidence.

## Release gates

Implementation may begin only from this contract. A local beta still requires:

1. canonical policy parsing and monotonic precedence tests;
2. platform-specific filesystem/process/network enforcement probes;
3. API runner component reservation and settlement coverage;
4. pinned official vendor SDK/CLI compatibility and auth-isolation probes;
5. terminal evidence for success and every failure class;
6. an accepted-patch evaluation corpus with cost and intervention metrics;
7. independent sandbox review before any hosted execution work.
