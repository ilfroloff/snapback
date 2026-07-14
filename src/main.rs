//! Thin `snapback` binary shim.
//!
//! All logic lives in the library crate so the module tree compiles once; this
//! binary and `src/bin/sb.rs` both just call [`snapback::run`], so `snapback`
//! and `sb` behave identically.

fn main() {
    snapback::run();
}
