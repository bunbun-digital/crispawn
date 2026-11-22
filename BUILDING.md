# Build CRISPAWN Community

Use an x86-64 Linux host, Python 3.12 or newer, Git, GCC/G++, GNU Make,
rsync, patch, file, wget, bison, flex, perl, bc, unzip, cpio, pkg-config,
dpkg-deb, kmod tools, sbsigntool, zstd and Pillow. On Ubuntu, the usual
Buildroot prerequisites plus `python3-pil python3-jsonschema kmod sbsigntool zstd`
provide these tools. Building requires Internet access to public upstreams;
the resulting community ISO has networking disabled.

```sh
git clone https://github.com/bunbun-digital/crispawn.git
cd crispawn
python3 tools/build.py
```

The builder retrieves the exact Buildroot commit, Rust 1.95.0 components,
the Buildroot-pinned Bootlin musl compiler, and Debian signed packages.
Checksums, the Debian kernel ABI and signatures are verified before composition.
The Rust Cargo.lock is mandatory. No preserved CRISPAWN ISO or private SDK is used.

Outputs are in `artifacts/perfect-backup-live/`. Logs, toolchains and temporary
builds are in ignored `build/` and `.orchestrator*/` directories. Allow at least
12 GiB free for the build and guest tests, and monitor free space. The default
ISO limit is 100,000,000 bytes. A download cache or clean upstream Buildroot
checkout may be reused with `--download-cache` and `--buildroot-source`; neither
contains product-specific binaries.

```sh
export PATH="$PWD/build/toolchain/bin:$PATH"
export CARGO_HOME="$PWD/build/cargo-home"
cargo test --locked --manifest-path examples/perfect-backup/rust/Cargo.toml
cargo fmt --all --check --manifest-path examples/perfect-backup/rust/Cargo.toml
python3 test/config/crispawn-config-contract.test.py
```

A functional guest test also needs QEMU, OVMF, ntfs-3g and mtools. Tests create
new ordinary disk image files and do not format physical devices.

```sh
python3 tools/test-crispawn-media-qemu.py --iso /path/to/the/generated.iso --work .orchestrator-test/unique-run
```

The ISO uses signed Debian kernel/shim/GRUB with Buildroot userspace. The
ISO build report records exact inputs and readback hashes. A successful build
is separate from actual Secure Boot, copy and restore acceptance.

