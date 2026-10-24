//! Builds libtess2, the tessellator of sprite meshes (`vendor/libtess2`), and
//! stb_image_resize, the texture resizer, and stb_image, the decoder of
//! images other than PNG (`vendor/stb`).
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
    // stb_image_resize, the texture resizer.
    cc::Build::new()
        .file("vendor/stb/stb_image_resize.c")
        .warnings(false)
        .compile("stbir");
    println!("cargo:rerun-if-changed=vendor/stb/stb_image_resize.c");
    println!("cargo:rerun-if-changed=vendor/stb/stb_image_resize.h");
    // stb_image, the decoder of the images other than PNG.
    cc::Build::new()
        .file("vendor/stb/stb_image.c")
        .warnings(false)
        .compile("stbi");
    println!("cargo:rerun-if-changed=vendor/stb/stb_image.c");
    println!("cargo:rerun-if-changed=vendor/stb/stb_image.h");
}
