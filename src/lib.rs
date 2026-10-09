//! MilerCast's engine: capture, encode and send, shared by the window (milercast.exe)
//! and the command line (milercast-cli.exe).
//!
//! Video never leaves the GPU:
//! capture (WGC) -> fit into a 16:9 canvas + NV12 (video processor) -> H.264 (hardware MFT).
//! Audio: WASAPI (desktop loopback + mic) -> mix -> AAC.
//! Both go out as FLV tags: to a file, live over RTMP, or both.

pub mod aac;
mod amf;
pub mod audio;
pub mod capture;
mod convert;
mod encoder;
pub mod engine;
mod flv;
mod gpu;
pub mod rtmp;
