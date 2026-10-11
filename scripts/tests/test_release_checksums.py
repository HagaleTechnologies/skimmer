"""Tests for the release job: SHA256SUMS and provenance (MAN-80), notes and image-failure recovery (MAN-298).

stdlib unittest only; workflows are read as text through test_ci_trust_boundary.py's helpers. The
static tests pin the release job's permissions and step order; the executed tests run the job's own
checksum step against fake archives and then the operator commands docs/RUNBOOKS/release.md prints.
Run: python3 -m unittest scripts.tests.test_release_checksums -v
"""
import fnmatch
import hashlib
import importlib.util
import os
import re
import shutil
import subprocess
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
_spec = importlib.util.spec_from_file_location("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))
wf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(wf)

WORKFLOW = "release-publish.yml"
RELEASE_WORKFLOWS = ("release.yml", "release-publish.yml")
RELEASE_PERMISSIONS = {"contents": "write", "id-token": "write", "attestations": "write"}
SIGNING_SCOPE = re.compile(r"^\s*(id-token|attestations)\s*:|write-all")
PINNED_ATTEST = re.compile(r"^actions/attest-build-provenance@[0-9a-f]{40} # v[0-9]+\.[0-9]+\.[0-9]+$")
DECISION_PATH_RE = re.compile(r"docs/DECISIONS/[0-9]{4}-[0-9]{2}-[0-9]{2}-man80-[A-Za-z0-9._-]*?\.md")
RUNBOOK = "docs/RUNBOOKS/release.md"
RUNBOOK_HEADING = "## Verifying a downloaded release"
RUNBOOK_LINK = RUNBOOK + "#verifying-a-downloaded-release"
RETRY_HEADING = "## If the image publish fails"
# The release's five archives, as the build matrix names them; sorted under any collation.
ARCHIVES = (
    "manta-linux-arm64.tar.gz",
    "manta-linux-x86_64.tar.gz",
    "manta-macos-arm64.tar.gz",
    "manta-macos-x86_64.tar.gz",
    "manta-windows-x86_64.zip",
)
DOWNLOADED = "manta-linux-x86_64.tar.gz"
# The commands the runbook gives operators; the executed tests below run exactly these.
LINUX_CHECK = "sha256sum -c --ignore-missing SHA256SUMS"
MACOS_CHECK = "shasum -a 256 -c --ignore-missing SHA256SUMS"
ATTEST_FLAGS = (
    "gh attestation verify ",
    "--repo HagaleTechnologies/manta",
    "--source-ref refs/tags/",
    "--signer-workflow HagaleTechnologies/manta/.github/workflows/release-publish.yml",
)
EXPR = re.compile(r"\$\{\{")
# Decision D7 (2026-09-06 broad review), MAN-298: what the notes of every release must say until
# manta clears its M2/M3 acceptance gates. Compared lowercased and whitespace-normalised.
D7_PHRASE = "pre-stability alpha, expect breakage"
# gh before 2.102.0 matched --source-ref case-insensitively and --signer-workflow as a prefix
# (GHSA-4mq3-hpgx-9cx8, GHSA-wjmr-j3rp-mh2g), so the runbook's guard must stop the verify for these.
GH_TOO_OLD = ("2.101.9", "2.99.0", "1.150.0", "DEV")
GH_NEW_ENOUGH = ("2.102.0", "2.110.1", "3.0.0")
FAKE_GH = """#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'gh version %s (2026-09-30)\\nhttps://github.com/cli/cli/releases/latest\\n' "$FAKE_GH_VERSION"
  exit 0
fi
echo "VERIFY RAN: $*"
"""


def executable(body):
    """A run: body without its shell comment lines."""
    return "\n".join(wf.code_lines(body or ""))


def release_steps():
    return wf.steps(wf.job_block(wf.read(WORKFLOW), "release"))


def uses(step):
    return wf.field(step, "uses") or ""


def with_field(step, key):
    block = wf.sub(step, "with")
    return None if block is None else wf.field(block, key)


def checksum_step():
    found = [s for s in release_steps() if "sha256sum" in executable(wf.run_body(s))]
    if len(found) != 1:
        raise AssertionError(f"{WORKFLOW} release job: expected one step running sha256sum, found {len(found)}")
    return found[0]


def index_where(steps, pred, what):
    found = [i for i, s in enumerate(steps) if pred(s)]
    if len(found) != 1:
        raise AssertionError(f"{WORKFLOW} release job: expected one {what} step, found {len(found)}")
    return found[0]


def publish_step():
    """The release job's softprops/action-gh-release step."""
    steps = release_steps()
    return steps[index_where(steps, lambda s: uses(s).startswith("softprops/action-gh-release@"), "Release")]


def fenced_commands(text):
    """Command lines of the ```sh / ```console blocks in text, `\\` continuations joined and a
    leading `$ ` prompt dropped."""
    commands, pending = [], ""
    for block in re.findall(r"^```(?:sh|console)\n(.*?)^```$", text, re.M | re.S):
        for line in block.splitlines():
            line = line.strip()
            if line.endswith("\\"):
                pending += line[:-1] + " "
                continue
            commands.append((pending + line).removeprefix("$ "))
            pending = ""
    return commands


def gnu_sha256sum():
    """Path of a GNU coreutils sha256sum, or None (macOS's BSD sha256sum is not the release runner's)."""
    exe = shutil.which("sha256sum")
    if exe is None:
        return None
    try:
        out = subprocess.run([exe, "--version"], capture_output=True, text=True).stdout
    except OSError:
        return None
    return exe if "GNU coreutils" in out else None


def archive_bytes(name):
    return f"fake {name} payload\n".encode("utf-8") + bytes(range(256))


def sums_text(names, data=archive_bytes):
    """The sha256sum text format: '<hex>  <name>' per file, in the order given."""
    return "".join(f"{hashlib.sha256(data(n)).hexdigest()}  {n}\n" for n in names)


def write_file(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def run(cmd, cwd):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)


class ReleaseJobTests(unittest.TestCase):
    def test_release_job_permissions_are_exactly_contents_id_token_and_attestations(self):
        job = wf.job_block(wf.read(WORKFLOW), "release")
        self.assertEqual(wf.field(job, "permissions"), "", "permissions must be a mapping, not write-all")
        perms = {}
        for line in wf.code_lines(wf.sub(job, "permissions")[1:]):
            if line.strip():
                key, _, value = line.strip().partition(":")
                perms[key.strip()] = value.strip()
        self.assertEqual(perms, RELEASE_PERMISSIONS)

    def test_steps_run_download_checksum_attest_then_release(self):
        steps = release_steps()
        download = index_where(steps, lambda s: uses(s).startswith("actions/download-artifact@"), "download")
        checksum = index_where(steps, lambda s: "sha256sum" in executable(wf.run_body(s)), "sha256sum")
        attest = index_where(steps, lambda s: uses(s).startswith("actions/attest"), "attest")
        publish = index_where(steps, lambda s: uses(s).startswith("softprops/action-gh-release@"), "Release")
        self.assertEqual(with_field(steps[download], "path"), "dist")
        self.assertEqual(with_field(steps[download], "merge-multiple"), "true")
        # Only the build legs' archives: docker/build-push-action's `*.dockerbuild` build record is
        # a run artifact too, and must not be checksummed, attested or published.
        self.assertEqual(with_field(steps[download], "pattern"), "manta-*")
        self.assertLess(download, checksum)
        self.assertLess(checksum, attest, "SHA256SUMS must exist before it is attested")
        self.assertLess(attest, publish, "fail closed: attest before the Release exists")

    def test_download_pattern_keeps_every_build_archive(self):
        build = wf.job_block(wf.read(WORKFLOW), "build")
        names = re.findall(r"^\s*(?:- )?artifact:\s*(\S+)\s*$", "\n".join(wf.code_lines(build)), re.M)
        self.assertEqual(sorted(n + (".zip" if "windows" in n else ".tar.gz") for n in names), list(ARCHIVES))
        for name in names:
            self.assertTrue(fnmatch.fnmatchcase(name, "manta-*"), f"pattern manta-* drops {name}")
        uploads = [s for s in wf.steps(build) if uses(s).startswith("actions/upload-artifact@")]
        self.assertEqual(len(uploads), 1)
        self.assertEqual(with_field(uploads[0], "name"), "${{ matrix.artifact }}")

    def test_checksum_step_runs_bash_inside_dist(self):
        step = checksum_step()
        self.assertEqual(wf.field(step, "shell"), "bash")
        self.assertEqual(wf.field(step, "working-directory"), "dist")
        self.assertIsNone(wf.field(step, "if"))
        self.assertIsNone(EXPR.search(wf.run_body(step)), "the step body takes no ${{ }} expressions")

    def test_attest_step_is_pinned_and_reads_sha256sums(self):
        steps = release_steps()
        step = steps[index_where(steps, lambda s: uses(s).startswith("actions/attest"), "attest")]
        self.assertRegex(uses(step), PINNED_ATTEST)
        self.assertEqual(with_field(step, "subject-checksums"), "dist/SHA256SUMS")
        self.assertIsNone(with_field(step, "subject-path"))
        self.assertIsNone(with_field(step, "push-to-registry"), "the release job pushes nothing to a registry")
        self.assertIsNone(wf.field(step, "if"))

    def test_release_upload_still_takes_all_of_dist(self):
        self.assertEqual(with_field(publish_step(), "files"), "dist/*", "SHA256SUMS rides the dist/* upload")

    def test_release_notes_open_with_the_pre_stability_label(self):
        step = publish_step()
        self.assertIn(D7_PHRASE, (with_field(step, "body") or "").lower(),
                      "the Release body must carry decision D7's label")
        self.assertEqual(with_field(step, "generate_release_notes"), "true",
                         "the body is prepended to the generated notes")

    def test_semver_prereleases_are_github_prereleases(self):
        self.assertEqual(with_field(publish_step(), "prerelease"), "${{ contains(github.ref_name, '-') }}",
                         "an -rc tag must not become the release that releases/latest serves")

    def test_only_the_newest_stable_tag_becomes_the_latest_release(self):
        # GitHub marks every new non-prerelease Release "Latest" unless told otherwise, so an
        # older-version tag released after a newer one would take over releases/latest.
        self.assertEqual(with_field(publish_step(), "make_latest"), "${{ needs.validate-tag.outputs.latest }}")
        gate = wf.job_block(wf.read(WORKFLOW), "validate-tag")
        self.assertEqual(wf.field(wf.sub(gate, "outputs"), "latest"), "${{ steps.latest.outputs.latest }}")
        step = wf.step_by_id(gate, "latest")
        self.assertIsNotNone(step, "validate-tag has no step with id: latest")
        self.assertEqual(wf.field(step, "if"), "github.event_name == 'push'")
        self.assertIn('scripts/release-version.sh is-newest-stable "$GITHUB_REF_NAME"', executable(wf.run_body(step)),
                      "the same recency predicate as publish-latest's :latest write")

    def test_no_other_release_job_can_mint_a_signing_token(self):
        for name in RELEASE_WORKFLOWS:
            text = wf.read(name)
            lines = text.splitlines()
            regions = {"(workflow)": lines[:lines.index("jobs:")]}
            for job in wf.job_ids(text):
                if (name, job) != (WORKFLOW, "release"):
                    regions[job] = wf.job_block(text, job)
            for where, block in regions.items():
                with self.subTest(workflow=name, job=where):
                    hits = [line for line in wf.code_lines(block) if SIGNING_SCOPE.search(line)]
                    self.assertEqual(hits, [])

    def test_workflow_names_an_existing_decision_record(self):
        refs = set(DECISION_PATH_RE.findall(wf.read(WORKFLOW)))
        self.assertTrue(refs, f"{WORKFLOW} never names the MAN-80 decision record")
        for ref in refs:
            self.assertTrue(os.path.isfile(os.path.join(ROOT, ref)), f"{WORKFLOW} names missing {ref}")


class OperatorDocsTests(unittest.TestCase):
    def read(self, rel):
        with open(os.path.join(ROOT, *rel.split("/")), encoding="utf-8") as f:
            return f.read()

    def test_readme_links_the_runbook_section(self):
        self.assertTrue(RUNBOOK_LINK in self.read("README.md"), f"README.md never links {RUNBOOK_LINK}")
        self.assertTrue(RUNBOOK_HEADING + "\n" in self.read(RUNBOOK), f"{RUNBOOK} has no {RUNBOOK_HEADING!r}")

    def test_runbook_gives_the_commands_these_tests_run(self):
        text = self.read(RUNBOOK)
        start = text.index(RUNBOOK_HEADING + "\n")
        end = text.find("\n## ", start + 1)
        section = text[start:] if end == -1 else text[start:end]
        for command in (LINUX_CHECK, MACOS_CHECK) + ATTEST_FLAGS:
            with self.subTest(command=command):
                self.assertIn(command, section)

    def test_runbook_says_how_to_retry_only_the_image_publish(self):
        text = self.read(RUNBOOK)
        self.assertIn(RETRY_HEADING + "\n", text, f"{RUNBOOK} has no {RETRY_HEADING!r}")
        start = text.index(RETRY_HEADING + "\n")
        end = text.find("\n## ", start + 1)
        section = text[start:] if end == -1 else text[start:end]
        reruns = [c for c in fenced_commands(section) if c.startswith("gh run rerun ")]
        self.assertTrue(any("--failed" in c for c in reruns), f"{RETRY_HEADING!r} gives no gh run rerun --failed")
        flat = " ".join(section.split())
        for needle in ("publish-latest", "docker buildx imagetools create"):
            with self.subTest(needle=needle):
                self.assertIn(needle, flat)

    def test_runbook_commands_name_the_repository_and_keep_every_platform(self):
        commands = fenced_commands(self.read(RUNBOOK))
        repo_commands = [c for c in commands if re.match(r"gh (run|release|workflow|attestation) ", c)]
        self.assertTrue(repo_commands)
        for command in repo_commands:
            with self.subTest(command=command):
                self.assertIn("--repo HagaleTechnologies/manta", command,
                              "gh outside a clone has no repository to default to")
        for command in commands:
            with self.subTest(command=command):
                self.assertFalse(command.startswith("docker pull "),
                                 "a single-platform engine pulls one platform of the multi-arch image")

    @unittest.skipIf(shutil.which("sh") is None or shutil.which("awk") is None, "needs sh and awk")
    def test_verify_block_refuses_gh_older_than_2_102_0(self):
        text = self.read(RUNBOOK)
        start = text.index(RUNBOOK_HEADING + "\n")
        blocks = [b for b in re.findall(r"^```sh\n(.*?)^```$", text[start:], re.M | re.S)
                  if ATTEST_FLAGS[0] in b]
        self.assertEqual(len(blocks), 1, f"{RUNBOOK}: expected one sh block running gh attestation verify")
        with tempfile.TemporaryDirectory() as tmp:
            gh = os.path.join(tmp, "gh")
            with open(gh, "w", encoding="utf-8") as f:
                f.write(FAKE_GH)
            os.chmod(gh, 0o755)
            for version, ok in [(v, False) for v in GH_TOO_OLD] + [(v, True) for v in GH_NEW_ENOUGH]:
                with self.subTest(gh=version):
                    env = dict(os.environ, PATH=tmp + os.pathsep + os.environ.get("PATH", ""),
                               FAKE_GH_VERSION=version)
                    proc = subprocess.run([shutil.which("sh"), "-c", blocks[0]], cwd=tmp, env=env,
                                          capture_output=True, text=True)
                    if ok:
                        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                        self.assertIn("VERIFY RAN: attestation verify", proc.stdout)
                    else:
                        self.assertNotEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                        self.assertNotIn("VERIFY RAN", proc.stdout, "an old gh must never run the verify")
                        self.assertIn("need gh 2.102.0 or newer", proc.stdout + proc.stderr)


class CiWiringTests(unittest.TestCase):
    def test_runs_in_required_test_job_on_unix_legs(self):
        wanted = "python3 -m unittest scripts.tests.test_release_checksums -v"
        steps = wf.steps(wf.job_block(wf.read("ci-full.yml"), "test"))
        matches = [s for s in steps if wf.field(s, "run") == wanted]
        self.assertEqual(len(matches), 1, f"ci-full.yml job test has no step running {wanted!r}")
        self.assertEqual(wf.field(matches[0], "if"), "runner.os != 'Windows'")


@unittest.skipIf(shutil.which("bash") is None or gnu_sha256sum() is None,
                 "needs bash and GNU coreutils sha256sum, as on the ubuntu-latest release runner")
class ChecksumStepExecutionTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.work = os.path.join(self._tmp.name, "work tree")  # a space in every path
        os.makedirs(os.path.join(self.work, "dist"))
        self.step = checksum_step()

    def tearDown(self):
        self._tmp.cleanup()

    def run_step(self, names):
        """Write fake archives into dist/ and run the step's own body as GitHub's `shell: bash` does."""
        for name in names:
            write_file(os.path.join(self.work, "dist", name), archive_bytes(name))
        script = os.path.join(self._tmp.name, "step.sh")
        with open(script, "wb") as f:
            f.write(wf.run_body(self.step).encode("utf-8"))
        cwd = os.path.join(self.work, wf.field(self.step, "working-directory") or ".")
        return run([shutil.which("bash"), "--noprofile", "--norc", "-eo", "pipefail", script], cwd)

    def read_sums(self):
        with open(os.path.join(self.work, "dist", "SHA256SUMS"), encoding="utf-8") as f:
            return f.read()

    def test_lists_every_archive_by_bare_name(self):
        proc = self.run_step(ARCHIVES)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        want = sums_text(sorted(ARCHIVES))
        self.assertEqual(self.read_sums(), want, "bare names, sorted, and never SHA256SUMS itself")
        self.assertEqual(proc.stdout, want, "the step log shows the published digests")
        check = run([gnu_sha256sum(), "-c", "--strict", "SHA256SUMS"], os.path.join(self.work, "dist"))
        self.assertEqual(check.returncode, 0, check.stdout + check.stderr)

    def test_operator_check_passes_for_a_download_and_fails_once_it_changes(self):
        proc = self.run_step(ARCHIVES)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        downloads = os.path.join(self._tmp.name, "my downloads")
        os.makedirs(downloads)
        for name in (DOWNLOADED, "SHA256SUMS"):
            shutil.copyfile(os.path.join(self.work, "dist", name), os.path.join(downloads, name))
        good = run(LINUX_CHECK.split(), downloads)
        self.assertEqual(good.returncode, 0, good.stdout + good.stderr)
        self.assertEqual(good.stdout, f"{DOWNLOADED}: OK\n")
        with open(os.path.join(downloads, DOWNLOADED), "ab") as f:
            f.write(b"\x00")
        bad = run(LINUX_CHECK.split(), downloads)
        self.assertNotEqual(bad.returncode, 0, "an altered archive must fail the check")
        self.assertIn(f"{DOWNLOADED}: FAILED", bad.stdout)

    def test_empty_dist_fails_the_step(self):
        proc = self.run_step(())
        self.assertNotEqual(proc.returncode, 0, "a Release with no archives must not get a SHA256SUMS")


@unittest.skipIf(shutil.which("shasum") is None, "shasum is not installed")
class MacosCheckTests(unittest.TestCase):
    def test_documented_macos_check_passes_and_fails_once_the_download_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            downloads = os.path.join(tmp, "my downloads")
            write_file(os.path.join(downloads, "SHA256SUMS"), sums_text(ARCHIVES).encode("utf-8"))
            write_file(os.path.join(downloads, DOWNLOADED), archive_bytes(DOWNLOADED))
            good = run(MACOS_CHECK.split(), downloads)
            self.assertEqual(good.returncode, 0, good.stdout + good.stderr)
            self.assertIn(f"{DOWNLOADED}: OK", good.stdout)
            with open(os.path.join(downloads, DOWNLOADED), "ab") as f:
                f.write(b"\x00")
            bad = run(MACOS_CHECK.split(), downloads)
            self.assertNotEqual(bad.returncode, 0, "an altered archive must fail the check")
            self.assertIn(f"{DOWNLOADED}: FAILED", bad.stdout)


if __name__ == "__main__":
    unittest.main()
