//! FloatingIslands [--max-hours N (0: no limit)] [--parent-pid PID] [--selftest] [--flicktest] [--render DIR]
//!                 [--render-lane DIR] [--viewer-shots OUT FIXTURES]
//! Data folder: ~/Library/Application Support/FloatingIslands (FLOATING_ISLANDS_DIR overrides it).
#![allow(non_snake_case)]

fn main() {
    islands::mac::main();
}
