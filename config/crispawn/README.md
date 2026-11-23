# CRISPAWN configuration contract

Original schemas, fixtures and validation code are Apache-2.0. See [runtime settings](../../docs/CONFIGURATION.md).

The JSON Schemas define the shared Builder/LiveOS wire formats. Community implements USB/NTFS and network.mode=none only. SMB/S3/Wi-Fi examples and future capability fixtures describe the shared protocol; they never prove support in a released ISO. Always inspect that ISO's actual capability metadata. Explicit unsupported features are rejected, never silently replaced by USB.

The ISO carries /boot/crispawn-media.json with media-layout schema 2, boot_iso_path /crispawn.iso, label CRISPAWN and directory crispawn. The boot ISO must use that exact filename at the ESP root; the external settings file is /crispawn-config.json. Credentials and personal settings must never be committed. The branded artwork is separately reserved under its asset LICENSE.
