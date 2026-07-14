//! Thin `sb` binary shim — the short alias for `snapback`.
//!
//! Identical to `src/main.rs`: it calls [`snapback::run`] with no `argv[0]`
//! dispatch, so `sb` and `snapback` are the same program under two names.

fn main() {
    snapback::run();
}
