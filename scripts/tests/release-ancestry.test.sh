#!/usr/bin/env bash
# MAN-244: real Git histories and release workflow dependencies, offline.
# Bash 3.2 compatible for the existing Ubuntu/macOS CI matrix.
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/check-release-ancestry.sh"
SCRATCH="$(mktemp -d)"
trap 'rm -rf "$SCRATCH"' EXIT
failures=0
fail() { printf 'FAIL: %s\n' "$*" >&2; failures=$((failures + 1)); }

# Isolate only this test process and its fixtures, never the real checkout.
# The cloud's injected remote.origin.fetch would otherwise break local clones.
unset GIT_CONFIG_COUNT GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR
unset GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_TERMINAL_PROMPT=0

accept() { # label, candidate, branch
  local label="$1"
  shift
  if ! "$SCRIPT" "$@" >"$SCRATCH/out" 2>"$SCRATCH/err"; then
    fail "$label: expected success"
    cat "$SCRATCH/err" >&2
  elif ! grep -Fq 'OK: release commit ' "$SCRATCH/out"; then
    fail "$label: missing success evidence"
  fi
}
reject() { # label, diagnostic, helper arguments...
  local label="$1" diagnostic="$2"
  shift 2
  if "$SCRIPT" "$@" >"$SCRATCH/out" 2>"$SCRATCH/err"; then
    fail "$label: expected rejection"
  elif ! grep -Fq "$diagnostic" "$SCRATCH/err"; then
    fail "$label: missing diagnostic '$diagnostic'"
    cat "$SCRATCH/err" >&2
  fi
  if [ -s "$SCRATCH/out" ]; then
    fail "$label: rejection emitted success output"
  fi
}

if [ ! -x "$SCRIPT" ]; then
  fail 'missing executable scripts/check-release-ancestry.sh'
else
  git init -q "$SCRATCH/source"
  cd "$SCRATCH/source"
  git config user.name fixture
  git config user.email fixture@example.invalid
  git config commit.gpgsign false
  git config tag.gpgsign false
  git symbolic-ref HEAD refs/heads/release/main
  git commit -q --allow-empty -m first
  first="$(git rev-parse HEAD)"
  git tag -a on-branch -m on-branch
  on_tag="$(git rev-parse refs/tags/on-branch)"
  git commit -q --allow-empty -m tip
  tip="$(git rev-parse HEAD)"
  git checkout -q -b divergent "$first"
  git commit -q --allow-empty -m divergent
  divergent="$(git rev-parse HEAD)"
  git tag -a off-branch -m off-branch
  off_tag="$(git rev-parse refs/tags/off-branch)"
  git checkout -q --orphan unrelated
  git commit -q --allow-empty -m unrelated
  unrelated="$(git rev-parse HEAD)"
  git checkout -q release/main
  git clone -q "file://$SCRATCH/source" "$SCRATCH/runner"
  cd "$SCRATCH/runner"

  accept 'default-branch tip' "$tip" release/main
  accept 'older reachable commit' "$first" release/main
  # Include all three checked values in the runtime evidence.
  for value in "$first" release/main "$tip"; do
    if ! grep -Fq "$value" "$SCRATCH/out"; then fail "missing evidence $value"; fi
  done
  accept 'annotated tag on branch' "$on_tag" release/main
  reject 'divergent commit' 'not an ancestor' "$divergent" release/main
  reject 'unrelated root' 'not an ancestor' "$unrelated" release/main
  reject 'annotated tag off branch' 'not an ancestor' "$off_tag" release/main
  reject 'missing object' 'cannot resolve' 0000000000000000000000000000000000000000 release/main
  tree="$(git rev-parse 'HEAD^{tree}')"
  blob="$(printf blob | git hash-object -w --stdin)"
  reject 'tree object' 'cannot resolve' "$tree" release/main
  reject 'blob object' 'cannot resolve' "$blob" release/main
  reject 'missing args' 'usage:'
  reject 'missing branch' 'usage:' "$tip"
  reject 'extra argument' 'usage:' "$tip" release/main extra
  reject 'empty commit' 'usage:' '' release/main
  reject 'empty branch' 'usage:' "$tip" ''
  reject 'mutable ref instead of object ID' 'object ID' HEAD release/main
  reject 'option instead of object ID' 'object ID' --help release/main
  reject 'malformed branch' 'invalid default branch' "$tip" 'release/../main'
  reject 'refspec injection' 'invalid default branch' "$tip" 'main:refs/tags/other'
  cd "$SCRATCH"
  reject 'outside repository' 'Git repository' "$tip" release/main

  git clone -q --depth=1 --branch on-branch "file://$SCRATCH/source" "$SCRATCH/shallow" 2>"$SCRATCH/clone.err"
  cd "$SCRATCH/shallow"
  [ "$(git rev-parse --is-shallow-repository)" = true ] || fail 'fixture must be shallow'
  accept 'shallow older valid tag' "$on_tag" release/main
  [ "$(git rev-parse --is-shallow-repository)" = false ] || fail 'history was not completed'

  cd "$SCRATCH/runner"
  accept 'populate accepting tracking ref' "$tip" release/main
  reject 'missing remote branch with stale accepting ref' 'fetch' "$tip" missing/branch
  git remote set-url origin "file://$SCRATCH/nonexistent"
  reject 'remote failure with stale accepting ref' 'fetch' "$tip" release/main
  git remote set-url origin "file://$SCRATCH/source"
  git -C "$SCRATCH/source" update-ref refs/heads/release/main "$unrelated"
  reject 'remote rewritten after accepting ref populated' 'not an ancestor' "$tip" release/main
  [ "$(git rev-parse refs/remotes/origin/man244-default)" = "$unrelated" ] || fail 'did not refresh default tip'
fi

cd "$REPO_ROOT"
job_block() {
  awk -v job="  $2:" '
    $0 == job { injob = 1; print; next }
    injob && /^  [A-Za-z0-9_-]+:/ { exit }
    injob { print }
  ' "$1"
}
require() { # text, fixed expected line, label
  if ! grep -Fxq -- "$2" <<< "$1"; then fail "$3"; fi
}
for wf in release-publish.yml release.yml; do
  path="$REPO_ROOT/.github/workflows/$wf"
  # MAN-241 may remove the duplicate, read-only workflow.
  if [ "$wf" = release.yml ] && [ ! -f "$path" ]; then continue; fi
  gate="$(job_block "$path" validate-tag)"
  build="$(job_block "$path" build)"
  require "$build" '    needs: validate-tag' "$wf: platform builds bypass validation"
  if grep -Eq '^    (if|environment):|continue-on-error:|\|\| true' <<< "$gate"; then
    fail "$wf: validation is skipped, environment-gated or ignores errors"
  fi
  require "$gate" '          fetch-depth: 0' "$wf: validation needs full history"
  step="$(awk '
    /      - name: Require release commit on the default branch/ { found = 1; print; next }
    found && /^      - / { exit }
    found { print }
  ' <<< "$gate")"
  require "$step" "        if: github.event_name == 'push'" "$wf: ancestry is not push-only"
  require "$step" '        shell: bash' "$wf: ancestry needs bash"
  require "$step" "          RELEASE_COMMIT: \${{ github.sha }}" "$wf: missing immutable event SHA"
  require "$step" "          DEFAULT_BRANCH: \${{ github.event.repository.default_branch }}" "$wf: missing event default branch"
  require "$step" "        run: scripts/check-release-ancestry.sh \"\$RELEASE_COMMIT\" \"\$DEFAULT_BRANCH\"" "$wf: helper not invoked"
  if [ "$wf" = release.yml ]; then
    require "$(job_block "$path" docker-build)" '    needs: build' "$wf: Docker build bypasses platform validation"
  fi
done

publish_path="$REPO_ROOT/.github/workflows/release-publish.yml"
dispatch="$(job_block "$publish_path" docker-build-dispatch)"
require "$dispatch" '    environment: ghcr-test-publish' 'both manual modes need unconditional approval environment'
require "$dispatch" "    if: github.event_name == 'workflow_dispatch'" 'manual job must be event-only, on any ref'
require "$dispatch" '    needs: [validate-tag, build]' 'dispatch must wait for validation and builds'
require "$dispatch" '      packages: write' 'manual package permission policy changed'
require "$dispatch" '        if: inputs.publish' 'manual registry login must be conditional'
require "$dispatch" "          push: \${{ inputs.publish == true }}" 'manual image push must be conditional'
require "$dispatch" "          tags: \${{ needs.validate-tag.outputs.image }}:\${{ needs.validate-tag.outputs.version }}" 'manual tag must come from validation'
require "$(job_block "$publish_path" validate-tag)" "            VERSION=\"dispatch-\${{ github.run_id }}\"" 'manual branch/tag must derive dispatch version'
for job in docker-publish-release publish-latest release; do
  block="$(job_block "$publish_path" "$job")"
  require "$block" "    if: github.event_name == 'push'" "$job: must require successful dependencies and a push event"
  if grep -Eq 'continue-on-error:|always\(' <<< "$block"; then
    # Strip comments, since the existing visibility-probe comment discusses always().
    active="$(sed '/^[[:space:]]*#/d' <<< "$block")"
    if grep -Eq 'continue-on-error:|always\(' <<< "$active"; then fail "$job: bypasses dependency success"; fi
  fi
  case "$job" in
    docker-publish-release) dependency='    needs: [validate-tag, build]' ;;
    publish-latest) dependency='    needs: [validate-tag, docker-publish-release]' ;;
    # MAN-298: the GitHub Release waits for the owner's approval, not for the image.
    release) dependency='    needs: [validate-tag, build]' ;;
  esac
  require "$block" "$dependency" "$job: dependency chain changed"
  require "$block" '    environment: ghcr-publish' "$job: missing production approval environment"
done
# MAN-298: a failed or re-run image publish must neither skip nor re-run the GitHub Release.
if sed '/^[[:space:]]*#/d' <<< "$(job_block "$publish_path" release)" | grep -q 'docker-publish-release'; then
  fail 'release: must not depend on docker-publish-release'
fi
require "$(cat "$REPO_ROOT/.github/workflows/ci-full.yml")" '        run: bash scripts/tests/release-ancestry.test.sh' 'CI must run ancestry suite'

if [ "$failures" -ne 0 ]; then
  printf '%s failure(s)\n' "$failures" >&2
  exit 1
fi
printf 'OK: all release ancestry and workflow cases passed.\n'
