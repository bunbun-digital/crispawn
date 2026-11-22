// SPDX-License-Identifier: Apache-2.0
fn main() {
    println!("cargo:rerun-if-env-changed=CRISPAWN_UI_LIB_DIR");
    if let Ok(path) = std::env::var("CRISPAWN_UI_LIB_DIR") {
        println!("cargo:rustc-link-search=native={path}");
    }
    println!("cargo:rustc-link-lib=dylib=crispawn-ui");
}
