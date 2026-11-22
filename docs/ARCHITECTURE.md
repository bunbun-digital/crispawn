# Community and Pro boundaries

The public repository owns disk selection and exclusions, whole-disk coverage,
compression, bounded files, BLAKE3, concurrent verification, failure handling,
GUI/CUI, USB/NTFS storage, boot integration and configuration contracts.

The separate private Pro repository pins this repository as a Git submodule.
It adds connection and remote-storage implementations without copying the
compression, verification or disk-safety engines. Existing network code was
moved there; SMB/S3 backends and a Pro ISO are future work.

The readiness interface in `engine/src/connection.rs` permits an application
to provide preparation and connection checks. Community uses a local no-op
implementation and contains no connection worker or unlock flag.
Its runtime rejects all explicit network modes and SMB/S3 destinations.
The Debian signed kernel and third-party driver licenses remain independent.

The public [CRISPAWN Builder](https://github.com/bunbun-digital/crispawn-builder)
repository is maintained separately. Boot configuration interfaces, versioned
archive contracts and the C UI ABI retain their existing identifiers; see
[integration compatibility](COMPATIBILITY.md). These identifiers are not product names.

