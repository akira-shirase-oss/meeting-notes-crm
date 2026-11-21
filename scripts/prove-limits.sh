#!/usr/bin/env bash
# Every limit is proven by a test that fails when the limit is removed.
#
# Each bound in the source carries a marker, `// LIMIT(name)`, on the line that sets it (or
# on a comment line just above it). For every marker this script removes the limit in a
# scratch copy — one substitution on that line — and runs the one test that proves it,
# which must then FAIL as a test (a build error is not a proof). A marker without a
# mutation below, or a mutation whose marker is gone, fails the script too, so a new limit
# cannot be added without its proof.
#
#   scripts/prove-limits.sh [--out FILE]   (FILE: a JSON record; never names the machine)
#
# Needs MEETCRM_TEST_DATABASE_URL (scripts/stack.sh env) for the database-backed tests.
# Builds into target/prove (this copy's own target directory); the scratch source is
# deleted at the end.
set -euo pipefail
case "${1:-}" in -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;; esac
cd "$(dirname "$0")/.."
here="$PWD"
out=""
case "${1:-}" in
  "") ;;
  --out) out="${2:?--out needs a file}" ;;
  *) echo "prove-limits.sh: unknown argument $1 (see --help)" >&2; exit 2 ;;
esac
: "${MEETCRM_TEST_DATABASE_URL:?is not set. Start the database and export its address: scripts/stack.sh up; eval \"\$(scripts/stack.sh env)\"}"

work="$(mktemp -d "${TMPDIR:-/tmp}/meetcrm-prove.XXXXXX")"
trap 'rm -rf "$work"' EXIT
tar --exclude ./target --exclude ./.git -cf - . | tar -xf - -C "$work"
export CARGO_TARGET_DIR="$here/target/prove"

# name|file|pattern=>replacement (a Python regex, applied to the marked line)|package|test target|test
mutations=(
  "connection-cap|crates/meetcrm-http/src/serve.rs|limits\.max_connections=>Semaphore::MAX_PERMITS|meetcrm-http|limits|connections_past_the_cap_wait_in_the_backlog_until_a_slot_frees"
  "header-read|crates/meetcrm-http/src/serve.rs|^.*$=>|meetcrm-http|limits|a_trickled_header_is_cut_off_and_never_answered"
  "requests-per-connection|crates/meetcrm-http/src/serve.rs|nth >= cap=>false|meetcrm-http|limits|pipelined_requests_past_the_cap_close_the_connection"
  "write-stall|crates/meetcrm-http/src/serve.rs|limits\.write_stall=>Duration::from_secs(3600)|meetcrm-http|limits|non_reading_clients_cannot_hold_connection_slots"
  "lifetime|crates/meetcrm-http/src/serve.rs|limits\.max_age=>Duration::from_secs(3600)|meetcrm-http|limits|a_busy_connection_is_closed_after_its_lifetime"
  "drain|crates/meetcrm-http/src/serve.rs|limits\.drain_timeout=>Duration::from_secs(3600)|meetcrm-http|limits|shutdown_lets_a_request_finish_and_cuts_at_its_deadline"
  "in-flight|crates/meetcrm-http/src/stack.rs|max_in_flight\.max\(1\)=>Semaphore::MAX_PERMITS|meetcrm-http|limits|requests_past_the_in_flight_gate_are_shed_at_once"
  "request-timeout|crates/meetcrm-http/src/stack.rs|limits\.request_timeout=>Duration::from_secs(3600)|meetcrm-http|limits|a_stalled_body_is_answered_by_the_request_deadline"
  "body|crates/meetcrm-http/src/stack.rs|limits\.max_body_bytes=>usize::MAX|meetcrm-http|limits|an_oversized_body_is_refused_with_the_envelope"
  "per-key-table|crates/meetcrm-http/src/budget.rs|^.*$=>|meetcrm-http|lib|budget::tests::the_table_never_grows_past_its_capacity_and_keeps_the_throttled"
  "sign-in-per-address|crates/meetcrm-accounts/src/signin.rs|self\.per_address\.take\(address\)\?;=>let _ = self.per_address.take(address);|meetcrm-accounts|budgets|one_address_flood_bounds_only_that_address"
  "sign-in-global|crates/meetcrm-accounts/src/signin.rs|self\.global\.take\(\)=>Ok::<(), RetryAfter>(())|meetcrm-accounts|budgets|a_distributed_flood_cannot_lock_out_a_trusted_owner"
  "mail-per-address|crates/meetcrm-accounts/src/signin.rs|self\.per_address\.take\(address\)\?;=>let _ = self.per_address.take(address);|meetcrm-accounts|budgets|mail_a_client_can_cause_is_bounded"
  "password-concurrency|crates/meetcrm-accounts/src/password.rs|concurrency\.max\(1\)=>Semaphore::MAX_PERMITS|meetcrm-accounts|lib|password::tests::at_most_the_concurrency_hashes_at_once"
  "plan|crates/meetcrm-accounts/src/domain.rs|current < i64::from\(limit\)=>true|meetcrm-accounts|limits|token_and_workspace_limits_hold_under_concurrency"
  "ready-deadline|crates/meetcrm-server/src/health.rs|self\.deadline=>Duration::from_secs(3600)|meetcrm-server|health|readyz_reports_a_silent_database_within_its_deadline"
  "probe-output|crates/meetcrm-media/src/probe.rs|json\.len\(\) > MAX_PROBE_OUTPUT=>false|meetcrm-media|lib|probe::tests::what_must_not_be_processed_is_refused_with_its_reason"
  "probe-streams|crates/meetcrm-media/src/probe.rs|out\.streams\.len\(\) > MAX_STREAMS=>false|meetcrm-media|lib|probe::tests::what_must_not_be_processed_is_refused_with_its_reason"
  "media-duration|crates/meetcrm-media/src/probe.rs|u128::from\(duration_ms\) > max_duration\.as_millis\(\)=>false|meetcrm-media|lib|probe::tests::what_must_not_be_processed_is_refused_with_its_reason"
  "upload-bytes|crates/meetcrm-media/src/store.rs|self\.bytes > self\.cap=>false|meetcrm-media|tools|a_receive_past_its_cap_is_refused"
  "output-total|crates/meetcrm-media/src/tools.rs|total > limits\.max_output_bytes=>false|meetcrm-media|tools|chunks_together_past_the_output_cap_are_refused"
  "ffmpeg-cpu|crates/meetcrm-media/src/tools.rs|\{cpu\}:\{\}\", cpu \+ 1=>3600:3601\"|meetcrm-media|tools|a_spinning_tool_is_killed_at_its_cpu_cap"
  "ffmpeg-memory|crates/meetcrm-media/src/tools.rs|\{\}\", limits\.max_memory_bytes=>unlimited\"|meetcrm-media|tools|a_recording_that_makes_the_tool_allocate_without_bound_is_refused_at_the_memory_cap"
  "ffmpeg-threads|crates/meetcrm-media/src/tools.rs|\"1\",=>\"0\",|meetcrm-media|tools|the_tools_run_single_threaded_with_bounded_probing"
  "probe-size|crates/meetcrm-media/src/tools.rs|PROBE_BYTES,=>\"5000000000\",|meetcrm-media|tools|the_tools_run_single_threaded_with_bounded_probing"
  "ffmpeg-output|crates/meetcrm-media/src/tools.rs|\{fsize\}=>unlimited|meetcrm-media|tools|a_transcode_past_its_output_cap_is_killed"
  "ffmpeg-deadline|crates/meetcrm-media/src/tools.rs|timeout\(deadline,=>timeout(Duration::from_secs(3600),|meetcrm-media|tools|a_hanging_tool_is_killed_at_its_deadline"
  "chunk-count|crates/meetcrm-media/src/tools.rs|index >= max=>false|meetcrm-media|lib|tools::segment_tests::a_closing_empty_segment_is_dropped_and_any_other_is_corrupt"
  "answer-bytes|crates/meetcrm-transcribe/src/import.rs|body\.len\(\) > MAX_ANSWER_BYTES=>false|meetcrm-transcribe|lib|import::tests::answers_that_break_a_bound_are_malformed"
  "utterances|crates/meetcrm-transcribe/src/import.rs|raw\.len\(\) > MAX_UTTERANCES=>false|meetcrm-transcribe|lib|import::tests::answers_that_break_a_bound_are_malformed"
  "utterance-text|crates/meetcrm-transcribe/src/import.rs|chars >= MAX_TEXT_CHARS=>false|meetcrm-transcribe|lib|import::tests::text_is_cleaned_and_bounded"
  "provider-upload|crates/meetcrm-transcribe/src/hosted.rs|size > MAX_AUDIO_BYTES=>false|meetcrm-transcribe|adapters|audio_past_the_upload_cap_is_never_sent"
  "provider-deadline|crates/meetcrm-transcribe/src/hosted.rs|self\.deadline=>Duration::from_secs(3600)|meetcrm-transcribe|adapters|a_provider_that_does_not_answer_times_out"
  "upload-idle|crates/meetcrm-meetings/src/routes.rs|s\.upload_idle=>std::time::Duration::from_secs(3600)|meetcrm-meetings|pipeline|media_bounds_a_stalled_upload_is_cut_at_the_idle_deadline"
  "uploads-at-once|crates/meetcrm-meetings/src/lib.rs|settings\.uploads_at_once\.max\(1\)=>Semaphore::MAX_PERMITS|meetcrm-meetings|pipeline|uploads_past_the_slot_limit_are_refused_busy"
  "uploads-per-workspace|crates/meetcrm-meetings/src/lib.rs|settings\.uploads_per_workspace\.max\(1\)=>usize::MAX|meetcrm-meetings|pipeline|one_workspace_cannot_take_every_upload_slot"
  "uploads-per-user|crates/meetcrm-meetings/src/lib.rs|settings\.uploads_per_user\.max\(1\)=>usize::MAX|meetcrm-meetings|pipeline|one_member_cannot_take_every_upload_slot"
  "upload-rate|crates/meetcrm-meetings/src/routes.rs|s\.upload_grace=>std::time::Duration::from_secs(3600)|meetcrm-meetings|pipeline|a_trickled_upload_is_cut_below_the_minimum_rate"
  "meeting-plan|crates/meetcrm-meetings/src/routes.rs|held >= cap=>false|meetcrm-meetings|plan|a_workspace_past_its_meeting_plan_is_refused"
  "media-plan|crates/meetcrm-meetings/src/routes.rs|stored\.saturating_add\(reserve\) > cap;=>false;|meetcrm-meetings|plan|a_workspace_past_its_storage_plan_is_refused_at_admission"
  "transcript-page-bytes|crates/meetcrm-meetings/src/routes.rs|text_bytes > MAX_PAGE_TEXT_BYTES;=>false;|meetcrm-meetings|pipeline|a_transcript_page_stops_at_its_byte_budget"
  "job-attempts|crates/meetcrm-meetings/src/worker.rs|< self\.inner\.settings\.max_attempts=>< u32::MAX|meetcrm-meetings|pipeline|provider_down_shows_transcription_unavailable_and_retries_within_bounds"
  "job-provider-deadline|crates/meetcrm-meetings/src/worker.rs|self\.inner\.settings\.provider_deadline=>Duration::from_secs(3600)|meetcrm-meetings|pipeline|a_provider_that_never_answers_is_cut_at_the_deadline_and_retried"
)

# Every marker in the source has exactly one mutation, and every mutation a marker.
found="$(grep -rhoE 'LIMIT\([a-z-]+\)' crates/*/src | sed 's/LIMIT(\(.*\))/\1/' | sort -u)"
defined="$(printf '%s\n' "${mutations[@]}" | cut -d'|' -f1 | sort -u)"
if [ "$found" != "$defined" ]; then
  echo "error: markers and mutations differ:" >&2
  diff <(echo "$found") <(echo "$defined") >&2 || true
  exit 1
fi

mutate() { # file marker pattern replacement
  python3 -I - "$work/$1" "$2" "$3" "$4" <<'PY'
import re, sys
path, marker, pattern, replacement = sys.argv[1:]
lines = open(path).read().split("\n")
tag = f"LIMIT({marker})"
changed = 0
for i, line in enumerate(lines):
    if tag not in line:
        continue
    target = i if line.split("//")[0].strip() else i + 1
    new = re.sub(pattern, replacement, lines[target], count=1)
    if new != lines[target]:
        lines[target] = new
        changed += 1
if not changed:
    sys.exit(f"the mutation for {marker} changed nothing")
open(path, "w").write("\n".join(lines))
PY
}

failures=0
results=()
for row in "${mutations[@]}"; do
  IFS='|' read -r name file subst package target test <<<"$row"
  pattern="${subst%%=>*}"
  replacement="${subst#*=>}"
  cp "$here/$file" "$work/$file"
  mutate "$file" "$name" "$pattern" "$replacement"
  if [ "$target" = lib ]; then selector=(--lib); else selector=(--test "$target"); fi
  started=$(date +%s)
  set +e
  output="$(cd "$work" && cargo test --locked -q -p "$package" "${selector[@]}" -- --exact "$test" 2>&1)"
  status=$?
  set -e
  seconds=$(( $(date +%s) - started ))
  cp "$here/$file" "$work/$file"
  if [ "$status" -ne 0 ] && grep -q 'test result: FAILED' <<<"$output"; then
    verdict=proven
    printf '  proven   %-24s %s fails without it (%ss)\n' "$name" "$test" "$seconds"
  elif [ "$status" -ne 0 ]; then
    verdict=broken
    failures=$((failures + 1))
    printf '  BROKEN   %-24s the mutation did not build or the test did not run:\n%s\n' "$name" "$(tail -20 <<<"$output")"
  else
    verdict=unproven
    failures=$((failures + 1))
    printf '  UNPROVEN %-24s %s passes without the limit\n' "$name" "$test"
  fi
  results+=("{\"limit\":\"$name\",\"test\":\"$test\",\"verdict\":\"$verdict\",\"seconds\":$seconds}")
done

if [ -n "$out" ]; then
  commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
  dirty=false; git diff --quiet HEAD -- . 2>/dev/null || dirty=true
  load="$(uptime | sed 's/.*load averages*: *//')"
  printf '{"proof":"limits","commit":"%s","tree_dirty":%s,"load_average":"%s","os":"%s","results":[%s]}\n' \
    "$commit" "$dirty" "$load" "$(uname -sr)" "$(IFS=,; echo "${results[*]}")" >"$out"
  echo "record: $out"
fi
if (( failures )); then
  echo "error: $failures of ${#mutations[@]} limits are not proven" >&2
  exit 1
fi
echo "limits ok: all ${#mutations[@]} limits are proven by a test that fails without them"
