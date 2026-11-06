//! NVIDIA GPU video on Windows: CUDA (D3D11 interop), NVENC (D3D11 input),
//! NVDEC. Adapted from `sard-linux`'s `nv` module; the struct layouts differ
//! where the SDK uses `long` (see `tools/nvlayout/gen.c`).

pub mod blob;
pub mod cuda;
pub mod layout;
pub mod nvdec;
pub mod nvenc;
