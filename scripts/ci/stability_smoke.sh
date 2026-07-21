#!/usr/bin/env bash
set -euo pipefail

echo "[stability-smoke] cross-chat permission matrix"
cargo test --quiet --test tool_permissions

echo "[stability-smoke] scheduler recoverability (restart persistence)"
cargo test --quiet --test db_integration test_scheduled_task_persists_across_db_reopen
cargo test --quiet test_replay_task_dlq_requeues_task_and_marks_replayed

echo "[stability-smoke] outbound delivery reliability contract"
cargo test --quiet -p microclaw-core sanitizes_private_reasoning_and_fake_tool_calls
cargo test --quiet -p microclaw-core split_text_respects_utf8_boundaries
cargo test --quiet -p microclaw-core split_text_preserves_every_byte_at_newline_boundaries
cargo test --quiet -p microclaw-storage test_chunked_delivery_resumes_and_stores_logical_message_once

echo "[stability-smoke] sandbox fallback and fail-closed behavior"
cargo test --quiet -p microclaw-tools test_router_falls_back_to_host_when_runtime_missing_and_not_required
cargo test --quiet -p microclaw-tools test_router_fails_closed_when_runtime_required_and_missing

echo "[stability-smoke] web inflight and rate-limit behavior"
cargo test --quiet test_same_session_concurrency_limited
cargo test --quiet test_rate_limit_window_recovers

echo "[stability-smoke] completed"
