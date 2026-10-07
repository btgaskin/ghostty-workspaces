# Validation for 0.2.0

Verified on an Apple Silicon Mac on 7 October 2026. Distinguish fixture acceptance, live provider transport and physical restart behavior.

## Executed checks

- `cargo fmt --all --check`
- `cargo clippy --offline --all-targets -- -D warnings`
- `cargo test --offline`: 49 active tests, with two opt-in diagnostic tests normally ignored.
- `cargo build --offline --release`
- Opt-in synthetic visual rendering and optimized fuzzy-search benchmark, described below.

Lifecycle fixtures verify exact Codex/Claude identities and scoped hooks, stale hooks, concurrent hook events, one-use provider launch gates, parent-loss cancellation before provider execution, and operation-aware resume through a real pseudo-terminal and Unix socket. Fake providers exercise the transport; they do not establish real conversation continuation.

Storage fixtures verify read-only inspection without initialization, stable IDs during legacy import, recovery from empty initialization, preservation/refusal of malformed or future schemas and conflicting legacy updates, durable run history, and receipts across stale/expired/reboot-invalidated operations. Launch guards reject missing/reserved dependencies and recorded live children even when an end flag is inconsistent.

Search fixtures verify bounded root metadata, private path exclusion from classifier payloads, confidence bands, backend revision cache separation, strict probability contracts, fuzzy ordering and summary-field coverage. A storage-error UI regression verifies that a failed rerank clears only its matching pending request and permits retry. Temporary utility fixtures verify cancellation/reaping and bounded pipes when a descendant retains a pipe.

## Appearance and local performance

Ratatui buffers were rendered at 140×42, 100×30, 80×24, 40×20 and 22×12. Wide, compact and narrow images were inspected. The [dashboard illustration](images/dashboard.png) uses synthetic work and machine readings; it is not a live system audit. Light-terminal contrast and real keyboard responsiveness still need physical acceptance.

An optimized local run matched and sorted 10,000 synthetic history entries over 40 iterations, with query `mem mon`: median **6.33 ms**, p95 **6.99 ms**. This includes fuzzy document construction, filtering and sorting. It excludes filesystem discovery, transcript parsing, rendering and model requests. It is one local microbenchmark, not a device-wide performance guarantee.

```sh
GWS_RENDER_DIR=/tmp/gws-render cargo test --lib ui::tests::render_review_artifacts -- --ignored
GWS_BENCHMARK_FILE=/tmp/gws-benchmark.json cargo test --release --lib performance_tests -- --ignored
```

## Live model transport

An explicit Jev request with three synthetic candidates succeeded in approximately **0.94 seconds**. The API reported model `jev-1.13.0`; all three low-confidence assessments retained fuzzy order. A repeated request used the cached timestamp. This verifies authentication, response validation, fallback policy and cache behavior; it does not establish ranking quality, model calibration or an independently verified model version.

An explicit headless Codex run with a short synthetic transcript succeeded using **gpt-6-luna**, medium reasoning, in approximately **9.6 seconds**. Structured purpose/progress/blocker/next-step output was recorded. No real work transcript was submitted for this validation.

Credentials were read from an existing private `.env` file. The repository and saved search configuration contain no copied key; configuration records only a path. Model jobs are on request and profiling pause blocks new jobs and cancels tracked temporary utilities.

## Installed setup

The release binary was installed with a previous-binary backup. Migration of the existing local register preserved every saved work ID and run token, and the original JSON backup was compared with the pre-migration state. Installed `doctor` and read-only `list` succeeded; host-authorized automation reported Ghostty 1.3.1 and the optional macmon binary. No tabs were restarted or stopped for this check, and ambiguous bindings remain unresolved.

## Acceptance limits

Physical reboot, continuation of real Codex/Claude/Cursor conversations after reboot, real-provider hook trust, and optional remote-device monitoring have not been acceptance-tested here. No user providers or unrelated processes were stopped during validation. Provider shutdown remains manual-only; declared service ownership is required for automated stopping. Shared provider servers and other applications remain outside profiling-pause control.

The local resident/hot-loaded classifier is an extension point, not an implemented model backend. Cursor private transcripts and subagents remain unavailable without a supported adapter or explicit export. Split geometry and scrollback are not restored.
