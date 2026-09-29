//! Prints `web/src/worker/layout.ts` on stdout for `cargo xtask wasm`, so `xtask` needs no
//! dependency edge to `pemu-wasm`.

fn main() {
    print!("{}", pemu_wasm::tsgen::layout_ts());
}
