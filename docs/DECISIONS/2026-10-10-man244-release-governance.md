# MAN-244: owner-approved releases from default-branch history

Date: 2026-10-10
Status: implemented in the branch; owner activation and hosted acceptance pending.

## Decision

Release tags require the owner's authorization. Publication requires the
owner's deployment approval, and tag-triggered builds first prove that the
event commit belongs to the default branch. The complete settings contract,
activation order, rehearsals and rollback are in the
[release runbook](../RUNBOOKS/release.md#release-governance-and-activation-man-244).
Local validation does not close MAN-244's live acceptance or unblock MAN-84.

Keep `release-tags-owner-only` active on `refs/tags/v*`, restricting creation,
update and deletion with exactly `thagale` as bypass identity. Keep
`ghcr-publish` tag-only with the owner as sole required reviewer. Allow
self-review because the owner also creates the tag, and disable administrator
bypass. The runbook defines a sole-owner team fallback if direct user bypass
is rejected by the live API; no broad role bypass is permitted.

The manual package-write job uses a separate `ghcr-test-publish` environment
that allows all branches and tags and requires the same owner review, with
self-review allowed and administrator bypass disabled. This applies even to
`publish=false`, since the job still holds package-write permission. The
selected policy preserves feature-branch dry builds without widening the
production environment's ref policy. It avoids duplicating the Docker build
job just to split dry-run permissions. Owner review of this change confirms
the selected policy; no prior owner approval of that choice is claimed.
Configure both environments before merging or using the changed workflow.

## Ancestry and dependency checks

`scripts/check-release-ancestry.sh` accepts the immutable event object ID and
the event's default-branch name through step environment variables. It peels
the object to a commit, fetches that exact branch, completes shallow history
if needed, and checks ancestry against the fetched tip. A missing object,
invalid input, fetch failure or failed Git comparison blocks validation even
if an old local tracking ref would have accepted the commit. Success logs
both commit IDs and the branch name. Older reachable commits are allowed.

Both retained release workflows invoke the helper in `validate-tag` before
any build. Only that step is push-only; the validation job still runs for
branch and tag dispatches. MAN-241 may remove the duplicate read-only
workflow later. Version grammar, prerelease behavior, cache policy and
`latest` ordering remain governed by MAN-65 and their existing implementation.

Production image publication retains `ghcr-publish`. GitHub Release creation
depends on successful image publication, so it waits for that approval too.
The separate `publish-latest` job retains its own environment protection and
may request another approval. Manual dispatch keeps `dispatch-<run_id>` and
never creates a version release or updates `latest`.

> **Partly superseded by MAN-298** (2026-10-11-man298-first-release.md): the Release still waits for this approval but no longer for successful image publication.

## Evidence correction and trust limits

Earlier MAN-244 research inferred that no actor could bypass the tag ruleset
because `bypass_actors` was absent in the response. That inference is invalid:
GitHub omits that property without ruleset write access. The caller's
`current_user_can_bypass: never` says nothing about the owner's bypass. The
owner must read the complete settings with adequate access, preserve correct
values, and repair only verified differences. The API supports `User` with
a numeric actor ID. [GitHub repository rules API](https://docs.github.com/en/rest/repos/rules#get-a-repository-ruleset).

Ancestry proves default-branch membership at check time, relying on that
branch's review controls. Tags use the workflow at their own commit; an old
commit may carry no guard. The owner must select a reviewed commit containing
this change. Owner-only tags and deployment review remain independent controls.

MAN-66's accepted residual risk remains: a write-capable identity can add a
workflow without these in-file gates and request a package-write
`GITHUB_TOKEN`. Repository default permissions do not cap explicit workflow
permissions. Moving the credential and the organization-wide app-permission
audit are outside MAN-244. This change replaces the earlier no-reviewer
policy for these publishing jobs, without claiming that residual risk is closed.

## Verification and rollback

Offline tests use real Git histories for reachable, divergent, unrelated,
annotated and shallow cases, invalid inputs, stale-ref fetch failures and a
changed remote tip. Workflow assertions preserve the validation dependencies,
push-only ancestry step and approval environment declarations. CI runs these
tests alongside the MAN-65 version suite on Ubuntu and macOS.

Live settings and owner/non-owner tag trials, insights, approval pauses and
hosted off-branch failures remain pending. No tags, deployments or admin
settings were changed by this implementation. Follow the runbook's rehearsals
before release activation; a passing local test cannot verify GitHub settings.

Rollback through a revert PR and deliberate owner restoration of captured
settings. Disabling tag rules or removing reviewers reopens the exposure.
Do not delete an environment still referenced by a workflow: subsequent runs
can recreate it without protection. Published artifacts are unaffected.
