# MAN-298: the first release (v0.1.0)

Date: 2026-10-11
Status: implemented in the branch; the tag push and hosted acceptance are the owner's.

manta had a release pipeline and no release, so the only way to run it was
`cargo install` from a clone. MAN-298 replaces MAN-84 (PR #126, closed
2026-10-06 and archived as tag `archive/MAN-84-pr126`). It prepares and
verifies everything up to the tag; release tags are owner-only (MAN-244),
so the owner pushes `v0.1.0`. Tagging now rather than after the Pi4
CPU-budget gate is decision D7 of
[the 2026-09-06 broad review](2026-09-06-broad-review-decisions.md).

## Decisions

### D1: the Release shares the image's approval, not its outcome

`release-publish.yml`'s `release` job has `needs: [validate-tag, build]` and
`environment: ghcr-publish`, and runs beside `docker-publish-release`
instead of after it.

- **Effects.** A failed image push no longer skips the GitHub Release
  (the ticket's third scenario). A re-run of the Docker job never re-runs
  `release`, because `release` is neither upstream nor downstream of it. A
  rejected deployment rejects both jobs, so nothing is published.
- **Rejected alternative.** PR #126 gated `release` on
  `!cancelled() && needs.build.result == 'success'`. GitHub fails a job
  whose deployment is rejected ("If a job is rejected, the workflow will
  fail", *Reviewing deployments*), so that gate would publish the Release
  after the owner **rejected** publication, undoing MAN-244. It would also
  re-run `release` whenever the Docker job is re-run, because re-runs repeat
  "all failed jobs and their dependents" (*Re-run workflows and jobs*).
- **Costs.**
  1. Re-running `release` alone, for example after an attestation failure,
     now asks for approval.
  2. The attestation's Fulcio certificate records `ghcr-publish` as the
     signing job's deployment environment (OID `1.3.6.1.4.1.57264.1.23`).
     The documented `gh attestation verify --source-ref … --signer-workflow
     … --deny-self-hosted-runners` command does not check that field: the
     same command exits 0 on cli/cli's release artifacts, which are signed
     in a job that sets `environment:`.
  3. GitHub's docs do not say whether one approval starts both waiting
     jobs. Reviews are per environment (`pending_deployments` takes
     `environment_ids`), so one is expected; the runbook tells the owner to
     approve again if asked and to record the prompt count.
- **Side benefit.** Binaries no longer wait for the multi-arch Docker
  build, which took about 24 minutes on 2026-10-05.

The environment keeps its name. `ghcr-publish` now gates the GitHub
Release too, but renaming it means new owner settings work and edits to the
MAN-244 guards.

### D2: the label

The `action-gh-release` step's `body:` opens with a blockquote,
"**Pre-stability alpha, expect breakage.** manta has not cleared its own
M2/M3 acceptance gates. …". `generate_release_notes: true` stays; GitHub's
create-release API prepends `body` to the generated notes. README's Status
section opens with the same phrase, and every archive ships README. Both
places are guarded case-insensitively and whitespace-normalised
(`scripts/tests/test_release_checksums.py`,
`crates/manta-cli/tests/docs_consistency.rs`).

### D3: SemVer pre-release tags become GitHub pre-releases

`prerelease: ${{ contains(github.ref_name, '-') }}`. `validate-tag` rejects
anything outside `vX.Y.Z[-pre]`, so `-` appears only in pre-releases. This
matches the `:latest` rule: without it a `v0.2.0-rc.1` would become GitHub's
"Latest", and README's `releases/latest/download/…` links would serve a
release candidate. `v0.1.0` stays a normal release, so the README badge and
`releases/latest` resolve to it.

### D4: README names no version

README downloads through `releases/latest/download/<archive>`, so it does
not go stale when the workspace version is bumped. A guard
(`readme_installation_offers_every_release_archive`) ties README's archive
list to the build matrix's `artifact:` entries.

### D5: CHANGELOG.md is promoted in the same change

`## [Unreleased]` entries move under `## [0.1.0] - 2026-10-11`, above an
empty `## [Unreleased]`, so the tagged tree carries the right CHANGELOG.
None of them is under `### Decoder output`, so `0.1.0` needs no version
bump. If the tag slips by days, the heading's date may differ from the tag
date by that much; the owner can amend it in review.

### D6: the MAN-244 positive approval rehearsal is v0.1.0 itself

Not a throwaway pre-release. A pre-release would leave a GitHub pre-release
and a GHCR version tag to clean up, and it exercises nothing `v0.1.0` does
not. The pre-approval failure modes (`validate-tag`, `build`) are covered
beforehand by the off-branch negative rehearsal and a `release.yml`
dispatch of the five-target matrix.

## Evidence (2026-10-11)

- `gh release list --repo HagaleTechnologies/manta` printed nothing,
  `git/matching-refs/tags/v` returned 0 refs, and
  `releases/latest/download/manta-linux-x86_64.tar.gz` returned 404.
- Live environments: `ghcr-publish` has `thagale` as required reviewer and
  administrator bypass still on (`can_admins_bypass: true`);
  `ghcr-test-publish` returned 404. The runbook requires bypass off and
  both environments configured before the workflow is used.
- The last five-target run was dispatch 37330523145 on `3986fb2`
  (2026-10-05), all green. MAN-244, MAN-268, MAN-80, MAN-83 and MAN-243
  landed on 2026-10-10, after it.
- At `9644a7f`, `cargo build --release -p manta-cli --features hpsdr`, then
  `scripts/package-release.py`, unpacked outside the checkout: `./manta
  --version` printed `manta 0.1.0 (git 9644a7f89371; features: hpsdr)`, and
  `gen` plus `decode` of V1 produced one spot.
- `gh attestation verify gh_2.99.0_linux_arm64.tar.gz --repo cli/cli
  --source-ref refs/heads/trunk --signer-workflow
  cli/cli/.github/workflows/deployment.yml --deny-self-hosted-runners`
  exited 0 on an artifact signed in a job with `environment:` set.

## Supersedes (in part)

- [MAN-244](2026-10-10-man244-release-governance.md): "GitHub Release
  creation depends on successful image publication, so it waits for that
  approval too." The Release still waits for the `ghcr-publish` approval,
  but no longer depends on the image.
- [MAN-80](2026-10-10-man80-release-checksums-attestation.md): "It has no
  `environment:`, so it adds no approval prompt, and re-running it does not
  re-run `docker-publish-release`." `release` now runs in `ghcr-publish`,
  so re-running it asks for approval; it still does not re-run
  `docker-publish-release`.

## Recovery

[`docs/RUNBOOKS/release.md`](../RUNBOOKS/release.md#if-the-image-publish-fails),
"If the image publish fails", says how to re-run only the image publish
(`gh run rerun <run-id> --failed --repo HagaleTechnologies/manta`). It
covers the three pitfalls PR #126's review left open: every `gh` command
carries `--repo` (or `GH_REPO`), a re-run also re-runs the jobs that depend
on the one named, and `:latest` is repaired with
`docker buildx imagetools create`, never pull and push, which on a
single-platform engine drops the other architecture.

## Deferred

1. CHANGELOG entries for PRs that merged after the file started without
   one: MAN-92, MAN-104, MAN-105, MAN-116, MAN-125. MAN-104 and MAN-105
   change which calls are spotted and as what type, which is decoder output
   under the CHANGELOG rule. There is no previous release to bump against,
   so `0.1.0` is unaffected.
2. MAN-83 deferred items 1 (the image's git SHA) and 3 (release notes built
   from CHANGELOG.md).
3. The stale "Wait for Codex Review" check name in `CLAUDE.md`,
   `release.yml` and `queue-retry-handler.yml`.
4. GHCR package visibility (MAN-66, MAN-65 finding 4).
5. macOS and Windows code signing; README documents the macOS quarantine
   workaround instead.
6. Renaming `ghcr-publish` if its name confuses.
