# Community releases

Publish ISO files as GitHub Release assets, never as Git objects. The public
repository contains only Community sources and no original private history.
Use a prerelease for initial hardware evaluation.

Before publication, build from a clean checkout with the pinned public inputs
in `sources.lock.json`, `Cargo.lock` and `config/secure-boot/debian-amd64.json`.
Record the source commit, command, input fingerprints, ISO size and SHA-256.
Keep the ISO below 100,000,000 bytes. Read the media marker and signed kernel,
shim and GRUB files back from the completed ISO and compare them with the
verified inputs. Do not modify signed binaries.

Run native tests, configuration contract tests and `tools/check-community.py`.
Run disposable QEMU image tests for USB copy, selected restore with readback,
NTFS logs, Secure Boot, optical BIOS/UEFI and expected configuration failure.
Record actual guest results independently of host fixtures. A QEMU result
does not establish Hyper-V or physical-device acceptance.

Attach these assets to the same release:

- Community x86-64 ISO and `SHA256SUMS`.
- CycloneDX 1.6 JSON generated and validated by the pinned tools in
  `config/sbom-tools.lock.json`, including the ISO hash.
- Dependency notices and copyrights extracted from the actual image and its
  exact Debian package inputs; preserve complete texts, including licenses
  embedded in Cargo crate archives and the Rust standard library notices.
- Corresponding source bundle: the public application snapshot, vendored
  `Cargo.lock` dependencies, exact Buildroot source and build configuration,
  target legal-info source archives and patches, and Debian source packages
  for the signed kernel, GRUB and shim **and their underlying code packages**.
- Test report with verified results and explicit untested platforms.

`config/secure-boot/debian-sources.json` pins the corresponding Debian source
files, including the signature wrappers' underlying Linux/GRUB/shim sources.
Verify every hash before packaging. Include the Buildroot source tree itself:
`make legal-info` alone omits skeleton sources and support patches. Do not
redistribute the external compiler binary as if it were source. Its musl and
GCC runtime notices are included separately in `LICENSES/` and the image.
Build-only tool inventories must remain distinct from the runtime SBOM.

Use native Cargo CycloneDX and Buildroot generators for dependency graphs,
Syft for the extracted image and Debian packages, and ScanCode for additional
reviewed copyright/license evidence. Use the pinned CycloneDX CLI to merge,
attach the artifact hash and validate; do not hand-serialize an SBOM. Preserve
raw tool output and inventory-coverage notes. Schema validation alone does not
establish inventory completeness or resolve license obligations.

Verify the uploaded hashes, public visibility and an unauthenticated ISO
download before announcing the release. Existing Builder releases, private
history, local settings, credentials, logs and disk images are not published.
Pro keeps a pinned Git submodule of this repository and stores its own network
extensions separately. Community has no edition-unlock switch or dormant
network connection backend.
