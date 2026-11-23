# Automated ISO builds with GitHub Actions

Assessment date: 2026-11-23. Status: feasible; the ISO workflow is not enabled yet.

GitHub-hosted Linux runners are the recommended first implementation for
`bunbun-digital/crispawn`. Build and test from the public checkout and pinned
public inputs. No SMB share, Windows relay, preserved ISO or private SDK is
needed. Use the existing signed Debian kernel/shim/GRUB without modifying or
signing them again; no private signing key is needed by this workflow.

## Current state

- [.github/workflows/ci.yml](../.github/workflows/ci.yml) currently runs Rust
  tests, configuration-contract tests and the Community boundary check.
  It does not build or publish an ISO.
- [tools/build.py](../tools/build.py) already fetches pinned public inputs,
  builds Buildroot userspace and composes the ISO in one command.
- [sources.lock.json](../sources.lock.json), Cargo.lock and the Debian input
  manifests pin inputs. The ISO composer verifies completed-image readback,
  media-layout metadata, signed-boot file hashes and the 100,000,000-byte limit.
- [The optical/USB guest suite](../tools/test-crispawn-optical-qemu.py)
  exercises Secure Boot, copy, restore/readback, NTFS logs, BIOS/UEFI and
  expected configuration failure. It uses QEMU TCG and does not require KVM,
  nested virtualization or physical disks.
- [tools/run-sbom-tools.py](../tools/run-sbom-tools.py) merges and validates
  tool-generated BOMs. Preparing its inputs, full notices and corresponding
  sources still needs a public, repeatable release-packaging entry point.
  Adding a workflow YAML alone would not automate the complete release.

## Runner resources and measured storage

The public-repository `ubuntu-24.04` runner currently provides 4 vCPUs, 16 GB
RAM and 14 GB SSD. Standard public runners have free compute usage; a hosted
job has a six-hour execution limit. These specifications are documented by
[GitHub's runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
and [Actions limits](https://docs.github.com/en/actions/reference/limits).
Artifact/cache storage is a separate allowance, not unlimited compute storage.

The completed alpha.2 build was inspected locally; these are allocated sizes,
not peak-space measurements:

| Item | Approximate allocated size |
| --- | --- |
| Buildroot userspace/output tree | 2.65 GiB |
| Official Rust toolchain | 0.79 GiB |
| Pinned input download cache | 0.62 GiB |
| Cargo registry, release target, signed profile and composition work | 0.55 GiB |
| Total for those build inputs/outputs | 4.61 GiB |
| Six-case guest-test output, separately | 0.85 GiB |

These totals omit native debug-test targets, SBOM tool installations, source
packaging, transient extraction and runner-preinstalled software. They do not
prove that all steps fit in one job. The guest suite requires more than 12 GiB
free initially and at least 10 GiB before each guest. Keep these guards.
Use fresh jobs for build, guest testing and license/source inventory; check
actual free space after dependency installation. If a standard runner cannot
meet the guards, investigate that job's measured usage before choosing a larger
runner. Do not silently weaken the guards or delete unrelated runner content.

The alpha.2 ISO is 32,440,320 bytes; its corresponding-source archive is
324,902,537 bytes. Both are well below the Release limit of 2 GiB per asset.
[GitHub Releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases)
are the long-term distribution location. Actions artifacts expire and require
GitHub sign-in to download; see the [artifact download documentation](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts).

## Recommended workflow

| Job | Work | Outputs |
| --- | --- | --- |
| Source tests | Existing Rust/configuration tests and Community boundary check for the exact source commit | Test summary |
| Build ISO | Ubuntu 24.04 prerequisites; `python3 tools/build.py --jobs 4 --release-id VERSION`; completed-image checks | ISO, SHA-256, source/input fingerprints, build report, compact inventory/source inputs |
| Guest tests | Fresh runner; download the exact ISO and verify its hash; run all six existing optical/USB cases | Acceptance JSON, relevant logs and screenshots |
| Inventory and sources | Fresh runner; run pinned native SBOM tools; extract actual-image notices and package complete corresponding sources | Validated CycloneDX JSON, notices archive, source archive, coverage report |
| Publish | Require all preceding jobs to pass for the same commit/ISO hash; upload and verify release assets | Complete GitHub Release and anonymous-download check |

Build ISO can run alongside source tests. Guest tests and inventory can run
in parallel after Build ISO. Publish must wait for all of them. Job names here
describe a proposal, not existing workflow files.

Use `ubuntu-24.04` explicitly, install host prerequisites from
[BUILDING.md](../BUILDING.md), and run Buildroot as the ordinary runner user.
Use the Rust components in the source lock for the production build. Installing
host dependencies needs sudo; building or testing does not require formatting
or mounting a physical disk.

For guest tests, install QEMU, OVMF with the Microsoft-enrolled variable image,
NTFS tools, partition utilities, xorriso, mtools, Pillow and independently
pinned BLAKE3/zstd verification dependencies. Existing helpers assume some
tools live under `build/os/host/bin`; allow explicit tool paths or transfer a
small verified host-tool bundle instead of the complete Buildroot SDK.

For inventory, install tools from
[config/sbom-tools.lock.json](../config/sbom-tools.lock.json) and verify their
hashes. The pinned ScanCode distribution needs Python 3.14; give it an isolated
environment instead of replacing Ubuntu's system Python. Preserve native
generator output and account for compound SPDX expressions and duplicate
Cargo binary BOM references during validated merging. Complete runtime-file
coverage remains a release gate, not merely JSON schema validity.

## Triggers, caches and retention

- Pull requests: keep fast source/configuration tests. Add an ISO check for
  relevant boot/build changes after the first hosted build is validated.
- Main-branch changes affecting the product/build: build a development ISO
  and run guest tests; keep artifacts briefly. Do not create a public Release
  for every commit.
- Version-tag push (`v*`): run the complete pipeline and publish the matching
  version only after test, source and license gates pass.
- `workflow_dispatch`: support an initial smoke build and bounded diagnostic
  reruns. A manually supplied version is validated, never executed as shell
  text. An untagged run does not publish a release.

Cache verified upstream downloads and Cargo registry archives using keys that
include the OS, architecture and lock hashes. Keep cache misses functional.
Do not make a cached product rootfs/SDK the authoritative build input. Pin
workflow actions to reviewed commits and keep publishing credentials out of
pull-request jobs. The ISO build does not need access to Pro sources.

Set explicit artifact retention. GitHub lists a 500 MB artifact allowance for
the Free organization tier and a default 10 GB repository cache allowance;
the organization's actual plan/current usage has not been checked. A source
archive of about 325 MB cannot be retained for many runs under 500 MB. Keep
large handoff artifacts briefly, bound development-ISO history, and retain
published versions in Releases. See [Actions storage limits](https://docs.github.com/en/actions/reference/limits)
and [artifact retention](https://docs.github.com/en/actions/tutorials/store-and-share-data).

## Release gates and implementation order

1. Add a manually triggered, read-only ISO build with a resource/duration
   report. Test with an empty cache on the actual hosted runner. Do not
   publish a new release during this feasibility run.
2. Move release inventory/source preparation into reviewed public tools;
   lock the independent verifier dependencies; make guest host-tool paths
   explicit. Verify that outputs need no local-only paths or shared files.
3. Connect the six-case guest suite and inventory jobs to the same ISO hash.
   Record disk usage, duration and any infrastructure failure separately.
4. Enable development builds and tag-based publication. Grant `contents: write`
   only to the final publication job. Validate version/tag/source identity;
   never overwrite an existing version with different bytes.
5. Publish the ISO, ISO checksum, SHA256SUMS, SBOM, complete notices,
   corresponding sources and test report together. Verify uploaded hashes and
   an unauthenticated ISO download, following [RELEASING.md](RELEASING.md).

A passing Actions/QEMU run establishes only its recorded guest acceptance.
Hyper-V and physical USB/CD checks remain separate platform validation.
Hosted cold-build time, peak usage, network reachability of pinned inputs and
the complete automated packaging path have not been measured in Actions yet.
The current ISO also contains build timestamps/random identifiers; automation
does not by itself guarantee bit-identical rebuilds from the same commit.
