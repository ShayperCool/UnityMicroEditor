//! Compiles the C sources linked into the tool.
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../native/nvenc/ume_nvenc.c");
    println!("cargo:rerun-if-changed=../../native/nvenc/nvEncodeAPI.h");
    cc::Build::new()
        .file("../../native/nvenc/ume_nvenc.c")
        .include("../../native/nvenc")
        .compile("ume_nvenc");
    println!("cargo:rustc-link-lib=dl");
}
