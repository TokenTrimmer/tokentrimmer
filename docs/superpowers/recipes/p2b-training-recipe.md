# P2b training recipe — RUNG 1 + RUNG 2 (off-repo, PyTorch+MPS, a TT-route teacher)

> **Owner: you (off-repo).** The training runs on the M4 Max (128GB, PyTorch+MPS).
> The input is the P1d TrainingCorpus (`tt export compress-corpus` output). The
> output is an ONNX model the `Scorer` loads via `TT_ML_MODEL_PATH`.

## Prerequisites
1. A P1d corpus export: `tt export compress-corpus --input capture.jsonl --output corpus.json`.
2. PyTorch+MPS on the M4 Max (`pip install torch transformers onnx onnxruntime`).
3. The teacher = a TT route pinned to a specific model (record `model_id` + `pinning_config` in the manifest). The teacher must be reachable from the training script (set `TT_API_KEY` + `TT_API_BASE`).

## RUNG 1 — warm-start (lossless structural, recall=1.0 provable)
**Input:** the lossless structural pairs (JSON/CSV/log rows; `kind != "prose"`).
**Label:** the `after` IS the `before` minified (the structural backends are content-preserving). This teaches the encoder token boundaries + the trivial-decision manifold (drop whitespace). Near-zero-information for the prose keep/drop task — it primes the encoder.
**Script shape:**
```python
# rung1_pretrain.py
import json, torch
from transformers import BertForTokenClassification, BertTokenizer

model = BertForTokenClassification.from_pretrained("bert-base-cased", num_labels=2)
tokenizer = BertTokenizer.from_pretrained("bert-base-cased")
model.to("mps")  # the M4 Max Metal backend

corpus = [json.loads(l) for l in open("corpus.json") if l.strip()]
# Filter the lossless structural pairs (kind != "prose").
rung1 = [p for p in corpus if p["kind"] != "prose"]
# Train: input=before, label=per-token keep/drop (mask from before vs after alignment).
optimizer = torch.optim.AdamW(model.parameters(), lr=5e-5)
for epoch in range(3):
    for pair in rung1:
        before = pair["before"]
        after = pair["after"]
        # Tokenize + create per-token keep/drop labels (1=keep, 0=drop).
        encoding = tokenizer(before, return_tensors="pt", truncation=True, max_length=512)
        labels = align_tokens(before, after, encoding)  # (alignment heuristic)
        encoding = {k: v.to("mps") for k, v in encoding.items()}
        labels = labels.to("mps")
        loss = model(**encoding, labels=labels).loss
        loss.backward()
        optimizer.step()
        optimizer.zero_grad()
    print(f"epoch {epoch}: loss={loss.item():.4f}")
torch.save(model.state_dict(), "rung1_weights.pt")
```

## RUNG 2 — teacher distillation (LLMLingua-2 REAL recipe)
**Input:** the prose `before` blocks (OpenAI-High confidence rows only — `billed_metric_tokens_removed == Some`). The export post-filter already guarantees these shipped.
**Label:** the teacher extracts an essence from the UNCOMPRESSED `before` + labels the original tokens by overlap. NOT targeting `CorpusPair.after` (P1b's 0.60 output) — that's circular = the headroom trap.
**Teacher = a TT route:**
```python
# rung2_distill.py — the teacher call
import os, requests, json, torch
from transformers import BertForTokenClassification, BertTokenizer

model = BertForTokenClassification.from_pretrained("bert-base-cased", num_labels=2)
model.load_state_dict(torch.load("rung1_weights.pt", weights_only=True))  # warm-start from RUNG1
model.to("mps")
tokenizer = BertTokenizer.from_pretrained("bert-base-cased")

corpus = [json.loads(l) for l in open("corpus.json") if l.strip()]
rung2 = [p for p in corpus if p["kind"] == "prose" and p.get("billed_metric_tokens_removed") is not None]

TT_API_KEY = os.environ["TT_API_KEY"]
TT_API_BASE = os.environ.get("TT_API_BASE", "https://api.tokentrimmer.com/v1")
TEACHER_MODEL = os.environ.get("TT_TEACHER_MODEL", "claude-sonnet-4-6")  # the pinned TT route

def teacher_essence(before: str) -> str:
    """Call the teacher via TT routing (the teacher is a pinned TT route)."""
    resp = requests.post(
        f"{TT_API_BASE}/chat/completions",
        headers={"Authorization": f"Bearer {TT_API_KEY}"},
        json={"model": TEACHER_MODEL, "messages": [
            {"role": "system", "content": "Extract the essence of the following text in 1-2 sentences. Preserve key facts, numbers, and identifiers verbatim."},
            {"role": "user", "content": before},
        ], "max_tokens": 100, "stream": False},
    )
    return resp.json()["choices"][0]["message"]["content"]

def token_overlap_labels(before: str, essence: str) -> list[int]:
    """Label each `before` token as 1 (keep) if it appears in the essence, 0 (drop)."""
    essence_tokens = set(essence.lower().split())
    before_tokens = before.split()
    return [1 if t.lower().strip(".,!?;:") in essence_tokens else 0 for t in before_tokens]

optimizer = torch.optim.AdamW(model.parameters(), lr=2e-5)
for epoch in range(5):
    for pair in rung2:
        before = pair["before"]
        essence = teacher_essence(before)  # the TT-route teacher
        labels = token_overlap_labels(before, essence)
        encoding = tokenizer(before, return_tensors="pt", truncation=True, max_length=512)
        labels_tensor = torch.tensor(labels[:encoding["input_ids"].shape[1]])
        encoding = {k: v.to("mps") for k, v in encoding.items()}
        labels_tensor = labels_tensor.to("mps")
        loss = model(**encoding, labels=labels_tensor).loss
        loss.backward()
        optimizer.step()
        optimizer.zero_grad()
    print(f"epoch {epoch}: loss={loss.item():.4f}")
torch.save(model.state_dict(), "rung2_weights.pt")
```

## ONNX export
```python
# export_onnx.py
import torch
from transformers import BertForTokenClassification, BertTokenizer

model = BertForTokenClassification.from_pretrained("bert-base-cased", num_labels=2)
model.load_state_dict(torch.load("rung2_weights.pt", weights_only=True))
model.eval()

# Dummy input for the ONNX export.
tokenizer = BertTokenizer.from_pretrained("bert-base-cased")
dummy = tokenizer("dummy input", return_tensors="pt")

torch.onnx.export(
    model,
    (dummy["input_ids"], dummy["attention_mask"]),
    "tt_learned_prose.onnx",
    input_names=["input_ids", "attention_mask"],
    output_names=["logits"],
    dynamic_axes={"input_ids": {0: "batch", 1: "seq"}, "attention_mask": {0: "batch", 1: "seq"}},
    opset_version=17,
)
print("exported tt_learned_prose.onnx")
```

## The reproducibility manifest
Save a JSON manifest alongside the model:
```json
{
  "teacher_model_id": "claude-sonnet-4-6",
  "teacher_pinning_config": "<the TT route's pinning config at training time>",
  "rung1_pairs": 1234,
  "rung2_pairs": 567,
  "onnx_exports": {
    "fp16_metal": "tt_learned_prose_fp16.onnx",
    "int8_cpu": "tt_learned_prose_int8.onnx"
  },
  "training_run_id": "<uuid>",
  "ts": "2026-07-08T00:00:00Z"
}
```

## Deploying the model
```bash
# On the M4 Max gateway (the owner-infra build with --features ml-scoring):
export TT_ML_MODEL_PATH=/path/to/tt_learned_prose.onnx
export TT_ML_SCORE_TIMEOUT_MS=50
# The gateway loads the model lazily on the first eligible Prose request;
# the shadow log (tt::compress::shadow) records the per-block delta.
```

## Notes
- The teacher call is a network call per training pair (a TT route). Cost: ~567 API calls × ~$0.001/call ≈ $0.57 total for RUNG 2.
- The M4 Max MPS backend trains the 110M model comfortably in 128GB unified memory.
- Re-certify the deployed numerics (FP16-on-Metal or INT8-CPU) through the quality judge BEFORE the ratchet trusts `prose-learned` (P2c).
- NEVER distribute the model weights — they're proprietary (the P1d training corpus + the weights are TT's exclusive asset).
