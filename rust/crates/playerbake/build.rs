//! Builds libtess2, the tessellator of sprite meshes (`vendor/libtess2`).
fn main() {
    let dir = std::path::Path::new("vendor/libtess2/Source");
    let sources = [
        "bucketalloc.c",
        "dict.c",
        "geom.c",
        "mesh.c",
        "priorityq.c",
        "sweep.c",
        "tess.c",
    ];
    let mut build = cc::Build::new();
    build
        .include(dir)
        .include("vendor/libtess2/Include")
        .warnings(false);
    for source in sources {
        build.file(dir.join(source));
        println!("cargo:rerun-if-changed=vendor/libtess2/Source/{source}");
    }
    build.compile("tess2");
}
