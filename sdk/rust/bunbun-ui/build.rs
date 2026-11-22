// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: LicenseRef-Bunbun-Proprietary
fn main() {
    println!("cargo:rerun-if-env-changed=BUNBUN_UI_LIB_DIR");
    if let Ok(path) = std::env::var("BUNBUN_UI_LIB_DIR") {
        println!("cargo:rustc-link-search=native={path}");
    }
    println!("cargo:rustc-link-lib=dylib=bunbun-ui");
}
