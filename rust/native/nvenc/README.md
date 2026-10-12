# NVENC encoder source

`ume_nvenc.c` is compiled by the crate's `build.rs` and linked into the tool. It
opens `libcuda.so.1` and `libnvidia-encode.so.1` at run time, so neither is a
build or start-up dependency.

`nvEncodeAPI.h` is the unmodified NVIDIA Video Codec SDK 12.1 interface header
from [FFmpeg/nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers)
(tag `n12.1.14.0`, `include/ffnvcodec/nvEncodeAPI.h`), under the MIT license
printed at its top. A newer header raises the oldest driver the encoder runs on.
