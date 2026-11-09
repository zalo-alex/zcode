//! The tier 2 guest binary (static musl), embedded by build.rs. Empty when it could not be built.
pub static GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/zcode-guest.bin"));
