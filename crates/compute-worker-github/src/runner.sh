#!/bin/sh
# Compute's GitHub Actions ephemeral runner worker script.
#
# Fetch the runner (HTTPS only, or a local archive), verify it, register it as
# an ephemeral runner, let it take exactly one job, and record what happened. Everything lives under the
# staged workspace Compute gives this execution and removed when it ends: no
# fixed paths, no background process (the runner runs in the foreground, in
# Compute's process group).
#
# The registration token arrives only as ACTIONS_RUNNER_INPUT_TOKEN, an
# environment variable the runner's config step reads. It is never an argument,
# never written to a file by this script, and unset before the runner starts.
set -u

work="${COMPUTE_WORK_DIR:-$PWD}"
out="${COMPUTE_OUTPUT_DIR:-$work/out}"
runner_dir="$work/actions-runner"
export HOME="$work/home"
mkdir -p "$HOME" "$runner_dir" "$out"

rc=1
exit_json=null
stage=start
started=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# The metadata is rewritten at every stage, so a runner that is killed (a
# timeout, a cancellation) still leaves the stage it reached and where its
# workspace was.
record() {
  stage=$1
  finished=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  job_name=""
  job_result=""
  # The listener prints these two lines to its terminal (actions/runner,
  # JobDispatcher.cs: `Running job: {name}` and `Job {name} completed with
  # result: {result}`); they are not in _diag. They are read from the copy of
  # the runner's output taken below.
  if [ -f "$work/runner.log" ]; then
    job_name=$(grep -h "Running job:" "$work/runner.log" 2>/dev/null | tail -n 1 | sed 's/.*Running job: *//' | tr -cd 'A-Za-z0-9 ._:/-' || true)
    job_result=$(grep -h "completed with result:" "$work/runner.log" 2>/dev/null | tail -n 1 | sed 's/.*completed with result: *//' | tr -cd 'A-Za-z0-9 ._:/-' || true)
  fi
  host=$(uname -sm | tr -cd 'A-Za-z0-9 ._-')
  printf '{"started_at":"%s","finished_at":"%s","exit_code":%s,"stage":"%s","work_dir":"%s","job_name":"%s","job_result":"%s","host":"%s","repository_url":"%s","runner_name":"%s","runner_version":"%s"}\n' \
    "$started" "$finished" "$exit_json" "$stage" "$work" "$job_name" "$job_result" "$host" \
    "$RUNNER_REPO_URL" "$RUNNER_NAME" "$RUNNER_VERSION" \
    > "$out/runner-metadata.json"
}

finish() {
  exit_json=$rc
  record "$stage"
  # Nothing the runner started outlives it. This script leads the execution's
  # process group, so TERM to the group ends every descendant that is still
  # running; ignoring TERM first keeps this script alive to return its status.
  trap '' TERM
  kill -TERM 0 2>/dev/null || true
}
trap finish EXIT
record start

die() {
  echo "runner worker: $1" >&2
  rc=$2
  exit "$rc"
}

archive="$work/runner.tar.gz"

record download
if [ -n "${RUNNER_ARCHIVE_FILE:-}" ]; then
  # An archive the caller already has (air-gapped hosts). It is verified below
  # exactly like a download.
  cp "$RUNNER_ARCHIVE_FILE" "$archive" || die "could not read the runner archive file" 70
else
  # HTTPS only, including redirects; the checksum below decides what runs.
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --silent \
    --show-error --location --output "$archive" "$RUNNER_DOWNLOAD_URL" \
    || die "could not download the runner archive" 70
fi

record verify
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$archive" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$archive" | cut -d' ' -f1)
else
  die "no sha256sum or shasum to verify the runner archive" 72
fi
[ "$actual" = "$RUNNER_SHA256" ] || die "runner archive checksum mismatch: refusing to run it" 71

record extract
tar -xzf "$archive" -C "$runner_dir" || die "could not extract the runner archive" 73
rm -f "$archive"

if [ "${RUNNER_INSTALL_DEPENDENCIES:-0}" = "1" ]; then
  record dependencies
  "$runner_dir/bin/installdependencies.sh" || die "installdependencies.sh failed" 74
fi

record configure
cd "$runner_dir" || die "runner directory missing" 73
set -- --unattended --ephemeral --disableupdate \
  --url "$RUNNER_REPO_URL" --name "$RUNNER_NAME" --work _work
if [ -n "${RUNNER_LABELS:-}" ]; then
  set -- "$@" --labels "$RUNNER_LABELS"
fi
./config.sh "$@" || die "runner registration failed" 75

# The token has done its job; the runner must not inherit it.
unset ACTIONS_RUNNER_INPUT_TOKEN

record run
# The runner's output goes to a file, which is printed when it ends and read
# for the job's name and result. (A pipe would keep this script waiting on any
# descendant that holds it open; a file cannot.)
./run.sh > "$work/runner.log" 2>&1
rc=$?
cat "$work/runner.log"
stage=done
exit "$rc"
