# Convenience targets. Wraps scripts/ + cargo/pnpm. Keep aligned with AGENTS.md.

.PHONY: help dev dev-down ci check test clippy fmt inspect-self loop \
        backlog backlog-take backlog-sync session-end review reflect \
        context-index context-for \
        loop-pause loop-resume loop-status \
        sessions plans \
        sync-licenses third-party-licenses licenses

help:
	@echo "TokenTrimmer dev targets:"
	@echo ""
	@echo "Build & test:"
	@echo "  make dev              Bring up local Postgres+Redis+MinIO+mailpit"
	@echo "  make dev-down         Stop local services"
	@echo "  make ci               Run the full local CI mirror"
	@echo "  make check            cargo check --workspace"
	@echo "  make test             cargo test --workspace"
	@echo "  make clippy           cargo clippy --workspace -- -D warnings"
	@echo "  make fmt              cargo fmt"
	@echo "  make inspect-self     Run our own Inspect against this repo"
	@echo "  make latency-smoke    Probe TT_GATEWAY_URL p50 latency (release gate; skips if unset)"
	@echo ""
	@echo "License compliance:"
	@echo "  make licenses             Sync per-crate LICENSE/NOTICE + regen THIRD-PARTY-LICENSES"
	@echo "  make sync-licenses        Copy root LICENSE+NOTICE into every publishable crate"
	@echo "  make third-party-licenses Regenerate THIRD-PARTY-LICENSES via cargo-about"
	@echo ""
	@echo "Autonomous-build harness:"
	@echo "  make backlog          List open P0/P1 backlog items"
	@echo "  make backlog-take     Print the next item to work on"
	@echo "  make backlog-sync     Sync backlog to GitHub Issues (autopilot label)"
	@echo "  make loop             One iteration of the autonomous build loop"
	@echo "  make session-end MSG=... TASK=... NEXT=..."
	@echo "                        Write HANDOFF.md + update STATE.md"
	@echo "  make reflect          Analyze the just-ended session for tuning signals"
	@echo "  make review           Generate this week's review report"
	@echo ""
	@echo "Context map (path of least resistance):"
	@echo "  make context-index    Regenerate .claude/INDEX.md from current code"
	@echo "  make context-for Q=<topic>"
	@echo "                        Find minimum context for a topic"
	@echo ""
	@echo "Loop control:"
	@echo "  make loop-pause       Pause the autonomous loop (touch .claude/PAUSED)"
	@echo "  make loop-resume      Resume after pause (rm .claude/PAUSED)"
	@echo "  make loop-status      Show whether loop is paused + current state"
	@echo "  make loop             Run one iteration manually"
	@echo ""
	@echo "History:"
	@echo "  make sessions         Browse the session archive (.claude/SESSIONS.md)"
	@echo "  make plans            List plans in .claude/plans/"

dev:
	docker compose -f docker-compose.dev.yml up -d
	@echo "Services up. Postgres on :5432, Redis on :6379, MinIO console on :9001, Mailpit UI on :8025."

dev-down:
	docker compose -f docker-compose.dev.yml down

ci:
	./scripts/ci-local.sh

check:
	cargo check --workspace --all-targets

test:
	cargo test --workspace --no-fail-fast

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

sync-licenses:
	./scripts/sync-licenses.sh

third-party-licenses:
	./scripts/gen-third-party-licenses.sh

licenses: sync-licenses third-party-licenses

inspect-self:
	./scripts/tt-inspect-self.sh

latency-smoke:
	./scripts/latency-smoke.sh

load-test-gateway:
	./scripts/load-test-gateway.sh

loop:
	./scripts/ralph-iteration.sh

backlog:
	./scripts/backlog.sh list

backlog-take:
	./scripts/backlog.sh take

backlog-sync:
	./scripts/backlog.sh sync

session-end:
	./scripts/session-end.sh "$(MSG)" --task "$(TASK)" --next "$(NEXT)"

reflect:
	./scripts/post-session-reflect.sh

review:
	./scripts/weekly-review.sh

context-index:
	./scripts/context-map.sh

context-for:
	@[ -n "$(Q)" ] || (echo "usage: make context-for Q=<topic>"; exit 1)
	./scripts/context-for.sh "$(Q)"

loop-pause:
	@touch .claude/PAUSED
	@echo "Loop PAUSED. Next iteration will exit silently."
	@echo "To resume: make loop-resume"

loop-resume:
	@rm -f .claude/PAUSED
	@echo "Loop RESUMED. Next iteration will proceed."

loop-status:
	@if [ -f .claude/PAUSED ]; then \
		echo "STATUS: PAUSED"; \
		echo "---"; \
		cat .claude/PAUSED; \
	else \
		echo "STATUS: ACTIVE (no .claude/PAUSED)"; \
	fi
	@echo "---"
	@echo "Current state:"
	@head -10 .claude/STATE.md 2>/dev/null || echo "(no STATE.md)"

sessions:
	@if [ -f .claude/SESSIONS.md ]; then \
		echo "Session archive (newest at bottom):"; \
		echo ""; \
		cat .claude/SESSIONS.md; \
	else \
		echo "No sessions archived yet. session-end.sh writes entries here."; \
	fi

plans:
	@if [ -d .claude/plans ]; then \
		echo "Plans in .claude/plans/:"; \
		ls -la .claude/plans/; \
	else \
		echo "No .claude/plans/ directory yet."; \
	fi
