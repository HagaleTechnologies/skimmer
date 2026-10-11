# Cutting a manta release

This is the operational runbook for the release pipeline
(`.github/workflows/release.yml`, `.github/workflows/release-publish.yml`).
It exists so a maintainer who has never cut a manta release can do so from
this document alone. See
`docs/DECISIONS/2026-09-05-man65-release-pipeline-hardening.md` for the
design rationale behind the behaviour described here.

## Release governance and activation (MAN-244)

This is the desired configuration, not proof that live settings have been
applied or rehearsals have passed. Owner activation and hosted acceptance
remain pending; the first release (MAN-298, which replaced MAN-84) waits
until the evidence below exists.
Local tests prove the helper and workflow wiring only.

Configure and verify settings first, merge the owner-reviewed workflow,
then rehearse and release. Both environments must be protected before this
workflow edit is merged or used. A workflow can create a named environment
without protection, so the YAML declaration alone provides no approval gate.
Owner review of the PR confirms the selected manual-path policy and the
change to MAN-66's earlier no-reviewer disposition.

### Settings contract

| Setting | Required state |
|---|---|
| Release tag ruleset | Existing `release-tags-owner-only`, reported ID `24550865`, verified by name/target before editing; active; target `tag`; include `refs/tags/v*`; no exclusions; creation, update and deletion restricted. Preserve unrelated rules and inspect applicable organization rules. |
| Bypass identity | Exactly `thagale`, resolved to the numeric user ID, with `actor_type: User`, `bypass_mode: always`. No broad admin/write role, integration, deploy key or extra user bypass. |
| `ghcr-publish` reviewers | Sole required reviewer `thagale`; self-review allowed because the owner also pushes the tag. |
| `ghcr-publish` ref policy | Preserve selected tag policy `v*`, without adding branch deployment permission. |
| `ghcr-test-publish` reviewers | Sole required reviewer `thagale`; self-review allowed; configure before any workflow references the new environment. |
| `ghcr-test-publish` ref policy | All branches and tags, preserving deliberate manual builds of arbitrary refs. Approval applies whether `publish` is true or false. |
| Both environments | Disable administrator bypass using Settings → Environments → the environment → uncheck `Allow administrators to bypass configured protection rules`. The documented REST update schema does not list `can_admins_bypass`, so do not depend on an undocumented PUT field. |

The rules API can omit `bypass_actors` unless the caller has ruleset write
access. An omitted field is not an empty list, and `current_user_can_bypass:
never` describes the caller only. Do not replace a concealed bypass list from
a permission-limited response. [GitHub repository rules API](https://docs.github.com/en/rest/repos/rules#get-a-repository-ruleset).

The owner first reads the complete ruleset with an administration-capable identity
and records the existing settings privately for rollback. If the bypass already
matches, leave it alone. The current REST schema supports direct user bypass; if
the live API rejects that type, use a non-secret team with exactly `thagale` as
its sole effective member, no inherited members, and one `Team` bypass in `always`
mode. This is a predetermined fallback, never a reason to grant a whole repository
role. Verify team membership during the trial. [GitHub ruleset
configuration](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/creating-rulesets-for-a-repository)

The owner then confirms the selected manual-path policy and MAN-66 note change
through code review. Settings can be applied before merge; at minimum both
environments must be protected before merging/using the workflow, and every
control must be verified before a release tag or manual package-write run.

### Rehearsal sequence

1. **Trial tag restrictions without triggering a release.** Create a temporary
   active tag ruleset for a unique `man244-rule-trial-<nonce>*` pattern with the
   same restrictions and bypass. These names match neither release trigger. Using
   existing appropriately authorized owner and non-owner identities, test
   creation, movement and deletion independently. The owner seeds tags needed for
   the negative update/delete attempts. Owner operations must succeed; non-owner
   operations must fail and leave refs unchanged. Record rule insight entries with
   actor, ref, operation, timestamp and rule ID. Do not substitute a read-only
   credential's ordinary push denial for evidence that the ruleset blocked a
   writer. Clean up the trial tags/rule as the owner.
2. **Verify production settings.** Read back the full production ruleset and both
   environment configurations. Preserve the protected `v*` pattern, its broader
   scope than the trigger, and the owner bypass. Confirm the environment reviewer
   and admin-bypass states. Export a sanitized evidence record or use owner
   screenshots; the cloud's permission-limited response is insufficient.
3. **Merge reviewed workflow changes.** Let the normal PR checks and owner review
   govern the merge. Confirm the target commit contains the guard, the dispatch
   environment declaration and the desired dependency chain. Do not tag an earlier
   commit whose workflow lacks the fix.
4. **Off-branch negative rehearsal.** From the reviewed commit, create a temporary
   branch with one harmless non-workflow change. The owner pushes a unique, valid
   prerelease tag such as `v<workspace-version>-man244.offbranch.<nonce>` pointing
   to it. It must match `v*`, `v[0-9]*` and the current SemVer validator. Observe
   `validate-tag` fail ancestry in every retained tag-triggered release workflow,
   with builds and all publication skipped. Record the run URL and failure/skip
   results. Owner deletes the tag/temporary branch after evidence capture. Never
   test using a commit that disables its own guard.
5. **Positive approval rehearsal: the first release.** MAN-298 uses the
   `v0.1.0` release itself, on a reviewed default-branch commit containing
   this fix, rather than a throwaway pre-release: a pre-release would leave a
   GitHub pre-release and a GHCR version tag to clean up and exercises
   nothing `v0.1.0` does not. Before approving, observe
   `docker-publish-release` and `release` waiting for `ghcr-publish`, and no
   image or Release for the tag. Confirm the run's commit and passing
   ancestry log. The owner approves `ghcr-publish`, handles any later prompt
   for `publish-latest`, and verifies the version image, the `latest` write
   and the GitHub Release. Record every approval identity and prompt count.
6. **Manual path.** Dispatch once with `publish=false` on a harmless feature
   branch and confirm its package-write job waits for `ghcr-test-publish`
   approval, then builds without a push. A deliberate `publish=true` trial must
   also wait, then publish only `dispatch-<run_id>` after owner approval. Neither
   run creates a GitHub Release or updates `latest`. Record run URLs and outcomes.

Do not close live acceptance on a settings screenshot alone. If rule insight
capture, owner bypass, pending deployment or off-branch failure cannot be
observed, preserve the pending gate and hold the first release (MAN-298).
Owner-controlled evidence should record the actual run/actor/ref/results; it
need not be committed by the cloud worker.

### Rollback and trust limits

Revert the workflow/helper change through a reviewed PR. The owner can restore
settings from the privately captured prior state; disabling the ruleset or
removing reviewers reopens the original exposure. Never delete an environment
while a workflow still references it: a later run can recreate it unprotected.
Rollback does not remove artifacts already published.

Ancestry checks membership in the default branch at validation time, relying
on that branch's review controls. A tag runs the workflow at its own commit,
so an older commit may lack the guard. Select a reviewed commit containing
this fix. A write-capable identity can introduce a different workflow that
omits these gates and requests a package-write `GITHUB_TOKEN`. MAN-66's
accepted credential-boundary risk and the app-permission audit remain outside
this change. See [the MAN-244 decision](../DECISIONS/2026-10-10-man244-release-governance.md).

## Cutting a release

1. Complete the activation checks above. Move `CHANGELOG.md`'s
   `## [Unreleased]` entries under `## [X.Y.Z] - YYYY-MM-DD`. If they
   include a `### Decoder output` section, X.Y.Z must bump at least the
   MINOR version over the previous release. A PATCH release never changes
   decoder output (see
   [the MAN-83 decision](../DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md)).
   `manta --version` and the JSON stream's `decoderVersion` carry the
   workspace version, so tag the release with that same version. If the
   release changes it, bump the workspace `version` in `Cargo.toml` through
   a reviewed PR and merge it to the default branch. The workflows do not
   compare the crate and tag versions, so keeping them equal is this step's
   job.
2. As `thagale`, select the reviewed default-branch commit containing the
   MAN-244 guard. Tag that explicit commit `vX.Y.Z` (see "Accepted tag
   grammar" below), then push: `git tag v1.2.3 <reviewed-commit-sha>` and
   `git push origin refs/tags/v1.2.3`. Only the owner may create, move or
   delete a release tag.
3. Pushing the tag triggers both workflows:
   - **`release.yml`** builds and packages all five targets (macOS
     x86_64/arm64, Windows x86_64, Linux x86_64/arm64) and validates the
     multi-arch Docker build (`push: false`) — this run never publishes
     anything; it is a build-only check of the tagged commit (it no longer
     runs on pull requests at all, since HAG-47).
   - **`release-publish.yml`** does the real work: `validate-tag` (checks
     tag grammar and default-branch ancestry before any platform build
     starts) → `build` (rebuilds the same five targets) → two jobs side by
     side, both in the `ghcr-publish` environment:
     `docker-publish-release` (pushes the multi-arch image to GHCR as
     `ghcr.io/hagaletechnologies/manta:X.Y.Z`, then `publish-latest` runs
     after it; see below) and `release` (writes `SHA256SUMS` over the five
     build artifacts, attests their build provenance, then creates the
     GitHub Release from the archives and `SHA256SUMS`; MAN-80, see
     "Verifying a downloaded release" below). The Release notes open with
     the pre-stability label above GitHub's generated list of merged pull
     requests. `release` does not wait for the image; see "If the image
     publish fails" below. The build-only workflow has the same ancestry
     guard before its platform builds.

   Both workflows build all five targets cold: no build cache is restored
   (MAN-243, `docs/DECISIONS/2026-10-10-man243-ci-trust-boundary.md`).
   Platform builds therefore take longer than a warm CI run. On 2026-10-05 a
   cold dispatch took 1m47s–6m11s per target, while the Docker job took
   about 24 min and still dominates.
4. When `docker-publish-release` and `release` wait for deployment
   approval, the owner verifies the run's commit and ancestry log, then
   approves `ghcr-publish`. GitHub reviews pending deployments per
   environment, so one approval should start both jobs; if it asks again,
   approve that too and record it. Rejecting publishes neither the image nor
   the Release. Until then, no version image or GitHub Release is published.
   A later `publish-latest` job may request a second approval; inspect and
   approve it too, without removing its protection. Record the actual prompt
   count.
5. Watch the `release-publish.yml` run's summary for the GHCR visibility
   warning and the `:latest`-not-updated warning — see below.
6. **Before announcing the release, run the clean-Windows check** below
   ("Manual check: the Windows ZIP starts without the Visual C++
   Redistributable"). CI cannot prove it.
7. **Before announcing the release, verify a download.** Once the Release
   is published, download one archive and `SHA256SUMS` from it and run both
   checks in "Verifying a downloaded release" below. The `release` job
   attests before it creates the Release, so if the attestation step
   failed, no Release exists: re-run the failed `release` job ("Re-run
   failed jobs"). It waits for `ghcr-publish` approval again; the Docker
   jobs do not run again. Also confirm the notes open with "Pre-stability
   alpha, expect breakage." above the generated list.

## If the image publish fails

The GitHub Release does not wait for the Docker image (MAN-298). Once the
owner approves `ghcr-publish`, `docker-publish-release` and `release` run
side by side, so a failed image push leaves the Release in place. The run
then shows `docker-publish-release` failed, `publish-latest` skipped and
`release` succeeded: the Release has its five archives and `SHA256SUMS`,
and GHCR has no `:X.Y.Z` image.

Do not delete or re-push the tag. The binaries are published and attested,
and a new tag is a new release. Retry only the image publish, from the
original run:

```sh
gh run list --repo HagaleTechnologies/manta --workflow release-publish.yml \
    --event push --limit 5 --json databaseId,headBranch,conclusion
gh run rerun <run-id> --failed --repo HagaleTechnologies/manta
```

Outside a clone, `gh` has no repository to default to: keep
`--repo HagaleTechnologies/manta` on every command, or set `GH_REPO`. The
**Re-run failed jobs** button on the run's page does the same as `--failed`.

- The re-run starts `docker-publish-release` again, then `publish-latest`,
  the job that depends on it.
  `gh run rerun --job <job-id> --repo HagaleTechnologies/manta` also
  re-runs the jobs that depend on the one you name (`gh`'s help calls them
  "dependencies"). Its job ID is the `databaseId` from
  `gh run view <run-id> --repo HagaleTechnologies/manta --json jobs`, not
  the number in the job's browser URL.
- `release` does not run again: it neither needs the Docker job nor is
  needed by it, so nothing re-uploads or re-attests the archives.
- `docker-publish-release` waits for `ghcr-publish` approval again, and so
  does `publish-latest`.
- A re-run uses the original commit and the original workflow file. A fix
  merged since then is not picked up: if the failure is in the workflow or
  the `Dockerfile`, fix it on the default branch and cut a new PATCH
  release.
- GitHub allows a re-run up to 30 days after the original run. After that,
  this version stays without an image; the next release publishes its own.

If `:latest` then needs repairing, copy the manifest list with
`docker buildx imagetools create` as "If `:latest` ends up wrong" shows. Do
not pull and push it: a single-platform Docker engine pulls only its own
architecture, and the pushed `:latest` would lose the other one.

## Manual builds and ancestry failures

A `workflow_dispatch` against either a branch or a tag keeps the version
`dispatch-<run_id>`. It skips ancestry validation and does not create a
version release or update `latest`. After the read-only platform builds,
`docker-build-dispatch` waits for the owner's `ghcr-test-publish` approval
in both modes, since the whole job holds package-write permission.
With `publish=false` it builds without login or push; with `publish=true`
it logs in and pushes only the dispatch image tag. This environment allows
all branches and tags, without widening production's tag-only policy.

An ancestry rejection blocks every dependent build, registry login, image
push, `latest` update and GitHub Release. Check `validate-tag`'s stderr:
non-ancestor means the selected commit is outside the freshly fetched
default branch; a fetch/object/history error means membership could not be
proved. Repair remote access or history errors and retry; do not bypass the
check or use an old local tracking ref as evidence. For an incorrect tag,
the owner corrects or deletes it and selects a reviewed commit containing
the guard. Prefer a new release version if artifacts were already published.

## Accepted tag grammar

Accepted: `vX.Y.Z` with an optional SemVer pre-release, e.g. `v1.2.3`,
`v0.1.0`, `v1.2.3-rc.1`.

**Rejected: SemVer build metadata** (`v1.2.3+linux`) and anything else that
isn't the shape above. `+` is legal in a Git ref and in SemVer, but is not
in Docker/OCI's tag grammar (`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`) — pushing
such a tag used to reach Buildx 30 minutes into the build, get rejected
there, and silently skip the GitHub Release along with it (MAN-65 finding
1). Today, `validate-tag` rejects it within seconds of the push, before any
of the five platform builds start, and the job's own log names the
accepted grammar. If this happens, the owner deletes the bad tag
(`git push origin :refs/tags/v1.2.3+linux`) and re-tags the reviewed commit
without the suffix. Non-owners must not attempt to bypass the tag rule.

The tag *trigger* itself is also narrower than a bare `v*` glob
(`v[0-9]*`), so an unrelated tag like `vendor-freeze` never invokes either
release workflow at all — but the trigger glob can't express the full
SemVer grammar, so `validate-tag` is still what gives a near-miss release
tag its explicit, readable failure.

## Pre-releases never become `:latest`

A pre-release tag (`v1.3.0-rc.1`) still publishes its own version tag
(`ghcr.io/hagaletechnologies/manta:1.3.0-rc.1`) but `publish-latest`
deliberately leaves `:latest` untouched — README's `docker run
ghcr.io/hagaletechnologies/manta:latest` install command must always hand
users a release, never a release candidate. They are also published as
GitHub pre-releases, so `releases/latest` (README's download links) never
serves one (MAN-298). The same recency check decides which GitHub Release
is "Latest": after builds, approval and attestation, `release` refreshes tags
and runs `is-newest-stable` immediately before passing the answer to
`make_latest`. A tag older than the newest stable tag seen by that check
publishes its Release without taking over `releases/latest`. A newer tag
pushed between the check and Release creation can still race.

## The one-time GHCR visibility step

GitHub Container Registry creates a brand-new container package with
**private** visibility on its first push, and there is no supported REST
endpoint reachable from a workflow's `GITHUB_TOKEN` to change that. This is
deliberately **not automated** — the only way to close it with the token
this workflow holds would be storing a long-lived personal access token
with `admin:packages` in the repo purely to flip one switch once, which is
a worse security posture than a single manual click (the same "flag it to
a human, don't silently work around it" disposition this repo already
applies to MAN-66, `.github/workflows/release-publish.yml`'s own header
comment).

What *is* automated is noticing: the `docker-publish-release` job's "Verify
the image is anonymously pullable" step (and its copy in
`docker-build-dispatch`, for a `publish=true` manual run) performs an anonymous pull probe on
every release — including a pre-release's first publish, which is when
this package is most likely to be created — and writes to the run's own
step summary:

- If the probe succeeds, an "OK" line.
- If it fails, a warning block with the exact click-path:
  1. `https://github.com/HagaleTechnologies/manta/pkgs/container/manta`
  2. **Package settings** → **Danger Zone** → **Change visibility** →
     **Public**

This step never fails the job — the release itself is fine even when the
package is still private; the fix is the out-of-band human action above.
Do this once, the first time a real tag is published; subsequent releases
push to the same already-public package and the probe reports "OK".

## Manual check: the Windows ZIP starts without the Visual C++ Redistributable

The Windows build links the MSVC C runtime statically (MAN-65 finding 2),
and every release build checks that: the "Assert the Windows binary is
statically CRT-linked" step fails the build if `manta.exe` imports
`VCRUNTIME140`, `MSVCP140` or any `api-ms-win-crt-*` DLL. What CI cannot
show is that the binary *starts* on a machine without the redistributable.
GitHub's `windows-latest` runner already has it installed, so a binary
that runs there proves nothing either way. This check is manual, and it
is the finding's real acceptance test. Do it for every release, before
announcing it:

1. Download `manta-windows-x86_64.zip` from the GitHub Release's assets.
2. Use a Windows x86_64 machine or VM that has never had the Visual C++
   2015-2022 Redistributable installed. Confirm that no copy of the runtime
   is reachable, from a PowerShell prompt opened in the folder you will
   unzip into:

   ```powershell
   where.exe vcruntime140.dll   # must find nothing
   ```

   `where.exe` searches the current folder and every `PATH` directory
   (System32 included), the same places Windows would load the DLL from.
   Other software can ship its own `vcruntime140.dll` on `PATH` without the
   redistributable being installed. If it finds any copy, this machine
   cannot tell a static build from a dynamic one. Use a different one.
3. Unzip the archive and run the binary from the unpacked
   `manta-windows-x86_64` folder:

   ```powershell
   .\manta.exe --help
   ```

   It must print manta's help text. A dialog or error naming
   `VCRUNTIME140.dll`, `MSVCP140.dll` or an `api-ms-win-crt-*` DLL means
   the release shipped a dynamically linked binary.

If the check fails, edit the GitHub Release to warn Windows users before
announcing it. Then look at the Windows entry's `rustflags` in the build
matrix of both `release.yml` and `release-publish.yml`, and at any other
place that sets rustflags for that build: Cargo takes rustflags from one
source and does not merge them, so a flag set somewhere else can replace
`+crt-static` without any error.

## If `:latest` ends up wrong

`publish-latest` re-checks, immediately before writing `:latest`, whether
its own tag is still the newest **tagged** stable release (comparing
against every `vX.Y.Z` *git tag* in the repo, numerically, ignoring
pre-releases) — this closes the race where two tags pushed close together
used to let the older build's `:latest` write win if it finished last
(MAN-65 finding 3).

This check is deliberately a Git-tag comparison, not a check of what GHCR
has actually published: if a numerically newer tag's own
`release-publish` run failed before reaching `docker-publish-release` (so
no image was ever pushed for it), this run still declines to write
`:latest` on that tag's account. A decline shows up as a `::warning` and a
`$GITHUB_STEP_SUMMARY` block on the `Is this still the newest stable
release?` step — watch for it the same way you watch for the GHCR
visibility warning above. See
`docs/DECISIONS/2026-09-05-man65-release-pipeline-hardening.md` ("a second
deliberate asymmetry") for why this is accepted rather than made to query
the registry.

The remaining timing window is narrow (seconds: the check happens right
after the multi-arch image is already pushed, and the `:latest` write is a
manifest-only copy, not a rebuild) but not zero — two tags pushed within
that window could still resolve out of order.

If `:latest` is ever wrong (from either of the above, or a manual
mistake), recovery is one command:

```sh
docker buildx imagetools create \
  -t ghcr.io/hagaletechnologies/manta:latest \
  ghcr.io/hagaletechnologies/manta:X.Y.Z   # the version that SHOULD be latest
```

Verify with:

```sh
docker buildx imagetools inspect ghcr.io/hagaletechnologies/manta:latest
docker buildx imagetools inspect ghcr.io/hagaletechnologies/manta:X.Y.Z
```

Both should report the same digest.

## What each artifact contains

- **Windows** (`manta-windows-x86_64.zip`): `manta.exe`, `README.md`,
  the two license files, and the unattended-service kit listed below. The
  binary links the MSVC C runtime **statically** (MAN-65 finding 2), so it
  needs no Visual C++ Redistributable installed — unpack and run.
- **macOS** (`manta-macos-{x86_64,arm64}.tar.gz`) and **Linux**
  (`manta-linux-{x86_64,arm64}.tar.gz`): the `manta` binary, `README.md`,
  the two license files, and the unattended-service kit. **Linux binaries
  need `libasound2` installed** (`sudo apt install libasound2` on
  Debian/Ubuntu/Raspberry Pi OS, or the
  equivalent ALSA runtime package elsewhere) — audio input is an
  unconditional dependency even for file/KiwiSDR/HPSDR-only use, and
  without it the binary fails to start.
- **Unattended-service kit** (MAN-268, every archive, assembled by
  `scripts/package-release.py`): `manta.example.toml`, `docker-compose.yml`,
  `packaging/README.md`, `packaging/systemd/manta.service`,
  `packaging/launchd/` (both plists, `create-service-account.sh`,
  `rotate-log.sh`) and `docs/RUNBOOKS/network-exposure.md`. See
  [docs/DECISIONS/2026-10-10-man268-unattended-packaging.md](../DECISIONS/2026-10-10-man268-unattended-packaging.md).
- **`SHA256SUMS`** (MAN-80, every Release): one `<sha256>  <archive>` line
  per archive above, bare file names, in `sha256sum` format. The same
  digests are the subjects of the Release's build-provenance attestation.
  See "Verifying a downloaded release" below.
- **Docker image** (`ghcr.io/hagaletechnologies/manta`): multi-arch
  (`linux/amd64`, `linux/arm64`), tagged `:X.Y.Z` for every release and
  `:latest` for the newest stable release only.

## Verifying a downloaded release

Every GitHub Release carries a `SHA256SUMS` file next to its five
archives, and a GitHub build-provenance attestation covers each archive
(MAN-80). Run both checks before you run a downloaded binary:

- **The checksum** catches a corrupted or swapped download: the file you
  have is byte-for-byte the file the Release published.
- **The attestation** proves `release-publish.yml` built the file from the
  release tag in this repository. Someone who can edit the Release can
  replace both an archive and `SHA256SUMS`, but cannot forge the
  attestation: it is signed through Sigstore with a certificate GitHub
  issues only to that workflow run.

### Checksum

Download the archive and `SHA256SUMS` from the same Release into one
folder and run the command for your system there. `--ignore-missing`
skips the archives you did not download.

```console
$ sha256sum -c --ignore-missing SHA256SUMS          # Linux
manta-linux-x86_64.tar.gz: OK
$ shasum -a 256 -c --ignore-missing SHA256SUMS      # macOS
manta-macos-arm64.tar.gz: OK
```

A line ending in `FAILED`, or `no file was verified`, means stop: do not
run the file. Download both files again; if the check still fails, report
it as described in [SECURITY.md](../../SECURITY.md).

On Windows, in PowerShell, from the download folder:

```powershell
$want = (Select-String -Path SHA256SUMS -SimpleMatch '  manta-windows-x86_64.zip').Line.Split(' ')[0]
$got  = (Get-FileHash manta-windows-x86_64.zip -Algorithm SHA256).Hash
if ($got -eq $want) { 'OK' } else { 'MISMATCH: do not run this file' }
```

(`-eq` ignores case, so `Get-FileHash`'s upper-case digest matches the
file's lower-case one.)

### Build provenance

This needs the [GitHub CLI](https://cli.github.com/) **2.102.0 or newer**,
signed in (`gh auth login`). Older versions compared `--source-ref`
case-insensitively and matched `--signer-workflow` against only the start
of the signer's identity
([GHSA-4mq3-hpgx-9cx8](https://github.com/cli/cli/security/advisories/GHSA-4mq3-hpgx-9cx8),
[GHSA-wjmr-j3rp-mh2g](https://github.com/cli/cli/security/advisories/GHSA-wjmr-j3rp-mh2g)).
On those versions, an attestation from a `V0.1.0` tag, or from a
workflow whose path starts with `release-publish.yml`, could pass the
check below. On any system, `gh --version` must report 2.102.0 or newer
before you trust the result.

Run this, with your file name and tag in place of
`manta-linux-x86_64.tar.gz` and `v0.1.0`. Its first command prints
`need gh 2.102.0 or newer` and stops before the check if `gh` is older:

```sh
gh --version | awk 'NR == 1 { have = $3; split(have, n, ".") }
    END { if (n[1] + 0 > 2 || (n[1] + 0 == 2 && n[2] + 0 >= 102)) exit 0
          print "need gh 2.102.0 or newer, found: " have; exit 1 }' &&
gh attestation verify manta-linux-x86_64.tar.gz --repo HagaleTechnologies/manta \
    --source-ref refs/tags/v0.1.0 \
    --signer-workflow HagaleTechnologies/manta/.github/workflows/release-publish.yml \
    --deny-self-hosted-runners
```

Success prints `✓ Verification succeeded!` and names
`.github/workflows/release-publish.yml@refs/tags/v0.1.0` as both the build
and the signer workflow. A file that is not the one the workflow built
fails with `Error: no attestations found`; a file built from any other ref
fails with `Error: expected SourceRepositoryRef to be refs/tags/v0.1.0,
got …`. Both exit non-zero.

Keep `--source-ref` and `--signer-workflow`. Only the owner can push a
release tag (MAN-244), so on `gh` 2.102.0 or newer, together they
reject an attestation made by a modified copy of the workflow run from a
branch. Why the job is shaped this way:
[docs/DECISIONS/2026-10-10-man80-release-checksums-attestation.md](../DECISIONS/2026-10-10-man80-release-checksums-attestation.md).
