# CRISPAWN Community development

- Original software is Apache-2.0. CRISPAWN logos and branded artwork are All rights reserved; preserve their separate LICENSE.
- Community supports local USB/NTFS only. Do not add connection workers, Wi-Fi credentials, network drivers, SMB/S3 backends or an edition-unlock switch to the community ISO.
- Keep source disk selection, boot/destination exclusions, whole-disk coverage, BLAKE3 and concurrent verification intact.
- Preserve existing sources, working changes and artifacts. Never reset or delete another run.
- Do not commit binaries, ISO/VHDX images, logs, credentials, private keys or local machine settings. Release assets are uploaded separately.
- Use disposable ordinary image files for tests. Never format a physical disk or change host firmware/security settings as part of tests.
- Build/test commands and dependency pins are in BUILDING.md. Verify actual guest evidence separately from host fixtures; missing evidence is not a pass.
- Preserve the signed Debian kernel, module ABI and shim/GRUB bytes.
- Public docs, tests and CI must run without the old private repository, its preserved ISO, SDK or Windows relay.

