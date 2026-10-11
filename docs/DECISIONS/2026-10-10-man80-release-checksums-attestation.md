# MAN-80: SHA256SUMS and build-provenance attestation on every release

Date: 2026-10-10
Status: implemented in the branch; hosted acceptance pending (MAN-84's first release).

## Decision

Every GitHub Release that `release-publish.yml` creates carries a
`SHA256SUMS` asset and a SLSA build-provenance attestation over its five
archives. Operators check a download with `sha256sum -c` (or `shasum` on
macOS, `Get-FileHash` on Windows) and `gh attestation verify`; the commands
are in the
[release runbook](../RUNBOOKS/release.md#verifying-a-downloaded-release),
and README's Installation notes point there.

- **Sign in `release`, not in each `build` leg.** The build legs run
  dependency build scripts and `cross` containers. Keeping `id-token: write`
  out of those jobs limits who can mint a signing token. The attestation
  still names the tag-triggered run of `release-publish.yml`.
- **The subjects are the five archives, read from `SHA256SUMS`**
  (`subject-checksums: dist/SHA256SUMS`), so the published file is the
  single source of the digests both checks use. `SHA256SUMS` itself is not
  a subject.
- **Fail closed.** The job writes `SHA256SUMS`, attests, then creates the
  Release. A failed attestation leaves no Release, rather than a Release
  operators cannot verify. Recovery is re-running the failed `release` job.
  It has no `environment:`, so it adds no approval prompt, and re-running it
  does not re-run `docker-publish-release`.

  > **Partly superseded by MAN-298**: `release` now runs in `ghcr-publish`, so re-running it asks for approval; it still does not re-run `docker-publish-release`.

- **`actions/attest-build-provenance`, pinned to v4.2.2 by SHA.** The ticket
  names it. Since v4 it is a thin wrapper over `actions/attest`, which
  upstream recommends for new code; both produce the same
  `https://slsa.dev/provenance/v1` predicate. The wrapper keeps its own
  `NODE_OPTIONS` header-size setting, and Dependabot's weekly
  `github-actions` updates keep the pin current.
- **The documented verify command pins `--source-ref refs/tags/<tag>` and
  `--signer-workflow HagaleTechnologies/manta/.github/workflows/release-publish.yml`**,
  plus `--deny-self-hosted-runners`. Release tags are owner-only (MAN-244),
  so this rejects an attestation made by a modified copy of the workflow
  dispatched on a branch. That holds only on `gh` 2.102.0 or newer, so
  the runbook chains a version check before the command. Older `gh`
  compared `--source-ref` case-insensitively and matched
  `--signer-workflow` as a prefix (GHSA-4mq3-hpgx-9cx8,
  GHSA-wjmr-j3rp-mh2g), so an attestation from a `V…` tag or from a
  workflow whose path starts with `release-publish.yml` could pass.

## Mechanism

The `release` job's permissions are exactly `contents: write`,
`id-token: write` and `attestations: write`. The download step now takes
only `pattern: manta-*`, the five build legs' artifacts.
`docker/build-push-action` also uploads a `*.dockerbuild` build record to
the run by default (`DOCKER_BUILD_RECORD_UPLOAD`). The unfiltered download
used before MAN-80 also fetched it. That action's README warns that this can
fail the download step; if the download succeeded, the record would be
published as a Release asset. With this change it would also be checksummed
and attested as a build output, so the download step now filters it out.
After the download step, a
`Write SHA256SUMS` step runs `sha256sum -- * > SHA256SUMS` with
`working-directory: dist`, so the file lists bare archive names. The glob
expands before the redirect creates the file, so it never lists itself;
an empty `dist/` fails the step on the literal `*`. The attest step reads
that file. The unchanged `softprops/action-gh-release` step's `files: dist/*`
uploads `SHA256SUMS` with the archives.

No `artifact-metadata: write`. The action's `create-storage-record` input
requires `push-to-registry: true`, and `actions/attest`'s plain-file
example grants only `id-token`, `contents: read` and `attestations`. This
job pushes nothing to a registry.

The repository is public, so attestations are signed through public-good
Sigstore (Fulcio certificate, Rekor transparency-log entry) at no plan
cost. A private repository would need GitHub Enterprise Cloud.

## Evidence, corrections and trust limits

Two corrections to the MAN-80 research, both reproduced before this change:

- `sha256sum dist/* > dist/SHA256SUMS` writes `dist/manta-…` names. In a
  download folder holding `SHA256SUMS` and one archive,
  `sha256sum -c --ignore-missing SHA256SUMS` prints `no file was verified`
  and exits 1. Running inside `dist/` fixes it;
  `scripts/tests/test_release_checksums.py` fails on the `dist/*` form.
- `artifact-metadata: write` is not needed, as above.

The checksum proves only that a download matches what the Release lists.
Someone who can edit the Release can replace both an archive and
`SHA256SUMS`. The attestation is the check they cannot forge: its
certificate is issued only to a run of `release-publish.yml` in this
repository, and `--source-ref` ties it to the tag. It proves where and from
which ref the file was built, not that the source at that tag is benign;
that still rests on review and MAN-244's owner-only tags. MAN-66's accepted
residual risk is unchanged: a write-capable identity can add a different
workflow, but its attestations name that workflow, which
`--signer-workflow` rejects on `gh` 2.102.0 or newer.

Out of scope, as follow-ons: attesting the Docker image, an SBOM,
`cargo binstall` metadata, a Homebrew tap, and `release.yml` (build-only;
it never publishes).

## Verification and rollback

`scripts/tests/test_release_checksums.py` runs in `ci-full.yml`'s required
`test` job on the Linux and macOS legs. It pins the job's permissions, the
download → checksum → attest → Release order, the SHA-pinned attest step
and its `subject-checksums` input, and `files: dist/*`. It also checks that
no other release job can request `id-token` or `attestations`. It runs the
job's own checksum step over fake archives (when GNU `sha256sum` is
present) and the runbook's Linux and macOS check commands against a good
and an altered download. It runs the runbook's `gh attestation verify`
block against fake `gh` versions and checks that a `gh` older than
2.102.0 never reaches the verify.

No `v*` release exists yet, so neither MAN-80 scenario has run end to end.
At MAN-84's first release, the owner confirms these and records the run URL
with MAN-84's evidence:

- `SHA256SUMS` is an asset.
- The run summary lists five attestation subjects.
- `sha256sum -c --ignore-missing SHA256SUMS` prints `OK` for a downloaded
  archive.
- The runbook's `gh attestation verify` block, version check included,
  prints `✓ Verification succeeded!` naming
  `release-publish.yml@refs/tags/<tag>`. Record the `gh --version` used;
  it must be 2.102.0 or newer.

Rollback is reverting the workflow change through a reviewed PR. That stops
new checksums and attestations only: Rekor entries are permanent, and
published Release assets stay until removed.
