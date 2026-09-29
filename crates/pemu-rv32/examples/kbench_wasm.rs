//! Browser entry point of benchmark workload K: `pemu_rv32::kbench`, the machine
//! `cargo xtask bench-k` runs natively, exported to JavaScript. `cargo xtask bench-browser` builds
//! it with the production core's `wasm-release` profile. Loading ([`kb_load`]) and running
//! ([`kb_run`]) are separate so the caller times the run alone. It is an example because a
//! benchmark entry has no place in the production core's frozen ABI.

use std::sync::{Mutex, MutexGuard, PoisonError};

use pemu_rv32::kbench::KernelMachine;

/// A `Mutex` static rather than a thread-local, which the core crates' `clippy.toml` denies; taken
/// once per call, never inside the timed run.
static MACHINE: Mutex<Option<KernelMachine>> = Mutex::new(None);

static ERROR: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A panic aborts on wasm32, so a poisoned lock is never observed there, and natively its value
/// is still the last one written.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fail(message: String) {
    *lock(&ERROR) = message.into_bytes();
}

#[unsafe(no_mangle)]
pub extern "C" fn kb_alloc(len: u32) -> *mut u8 {
    let mut buf = vec![0u8; len as usize].into_boxed_slice();
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Frees what [`kb_alloc`] returned.
///
/// # Safety
///
/// `ptr` must come from `kb_alloc(len)` and not be freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kb_free(ptr: *mut u8, len: u32) {
    // SAFETY: the caller promises ptr came from kb_alloc with this len, which leaked exactly this
    // boxed slice.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len as usize)) });
}

/// Returns 0, or -1 with the reason in [`kb_error`].
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kb_load(
    ptr: *const u8,
    len: u32,
    iters: u32,
    max_block_insns: u32,
) -> i32 {
    // SAFETY: the caller promises `len` readable bytes at `ptr`.
    let image = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let Ok(max_block_insns) = u16::try_from(max_block_insns) else {
        fail(format!("max_block_insns {max_block_insns} is over 65535"));
        return -1;
    };
    match KernelMachine::new(image, iters, max_block_insns) {
        Ok(machine) => {
            *lock(&MACHINE) = Some(machine);
            0
        }
        Err(e) => {
            fail(e);
            -1
        }
    }
}

/// Returns the instructions retired as an `f64`, so no BigInt crosses the boundary, or -1 with the
/// reason in [`kb_error`].
#[unsafe(no_mangle)]
pub extern "C" fn kb_run(slice: u32) -> f64 {
    let mut m = lock(&MACHINE);
    let Some(machine) = m.as_mut() else {
        drop(m);
        fail("kb_run before kb_load".to_string());
        return -1.0;
    };
    match machine.run(u64::from(slice.max(1))) {
        Ok(()) => machine.insns() as f64,
        Err(e) => {
            drop(m);
            fail(e);
            -1.0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn kb_checksum() -> u32 {
    lock(&MACHINE).as_ref().map_or(0, KernelMachine::checksum)
}

#[unsafe(no_mangle)]
pub extern "C" fn kb_slow_stores() -> f64 {
    lock(&MACHINE)
        .as_ref()
        .map_or(0.0, |m| m.slow_stores() as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn kb_translations() -> f64 {
    lock(&MACHINE)
        .as_ref()
        .map_or(0.0, |m| m.stats().blocks_built as f64)
}

#[unsafe(no_mangle)]
pub extern "C" fn kb_unload() {
    *lock(&MACHINE) = None;
}

/// UTF-8 reason of the last failure; its length is [`kb_error_len`].
#[unsafe(no_mangle)]
pub extern "C" fn kb_error() -> *const u8 {
    lock(&ERROR).as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn kb_error_len() -> u32 {
    lock(&ERROR).len() as u32
}
