//! The raw C ABI, with a frozen function list. No wasm-bindgen: hot data moves through the
//! `HostIo` rings and commands take a JSON cold path. Pointers are wasm32 linear-memory
//! addresses; `handle` is the `u32` payload of a successful `pemu_build`.
//!
//! - `pemu_new` never fails: a configuration that does not parse is reported by `pemu_build` as
//!   `E_USAGE`, because only a `res` call can carry the reason.
//! - `pemu_run` answers [`NO_MACHINE`], `pemu_now_ps` -1 and `pemu_io_layout` null for a handle
//!   that names no machine.
//! - `pemu_snapshot` accepts flags 0 only; exports go through the registry `snapshot` command,
//!   which applies redaction.

use std::alloc::{Layout, alloc, dealloc};

use pemu_api::error::{ApiError, E_USAGE};

use crate::instance::{self, MachineBuilder};
use crate::layout::{ABI_VERSION, IoLayout, ResultHeader, STATUS_OK};

/// Alignment of every buffer `pemu_alloc` hands out: 8, so a caller may place a `u64` cursor
/// block, an `i16` sample run or a `repr(C)` record batch in it.
const ALLOC_ALIGN: usize = 8;

fn byte_layout(len: usize) -> Option<Layout> {
    if len == 0 {
        return None;
    }
    Layout::from_size_align(len, ALLOC_ALIGN).ok()
}

/// Allocates `len` bytes the worker may write, or null. Used by [`pemu_alloc`] and every result
/// payload, so one free rule covers both.
fn alloc_bytes(len: usize) -> *mut u8 {
    match byte_layout(len) {
        // SAFETY: the layout has a non-zero size.
        Some(layout) => unsafe { alloc(layout) },
        None => core::ptr::null_mut(),
    }
}

/// Frees what [`alloc_bytes`] returned for the same `len`.
///
/// # Safety
///
/// `ptr` must be null, or a pointer [`alloc_bytes`] returned for exactly this `len` and not yet
/// freed.
unsafe fn free_bytes(ptr: *mut u8, len: usize) {
    if let (false, Some(layout)) = (ptr.is_null(), byte_layout(len)) {
        // SAFETY: the caller promises ptr came from alloc_bytes with this len, so the layout is
        // the one it was allocated with.
        unsafe { dealloc(ptr, layout) }
    }
}

/// Where a result's payload starts inside the block `pemu_result_free` frees: past the 12-byte
/// header, rounded up to [`ALLOC_ALIGN`]. One allocation on purpose: the `u32`
/// `ResultHeader::ptr` cannot hold a 64-bit host address, so the free derives the payload from
/// the header's own address.
const RESULT_PAYLOAD_OFFSET: usize = 16;

fn result_layout(len: usize) -> Option<Layout> {
    Layout::from_size_align(RESULT_PAYLOAD_OFFSET + len, ALLOC_ALIGN).ok()
}

/// A result header owning a copy of `payload` in one block; freed with [`pemu_result_free`].
fn result(status: u32, payload: &[u8]) -> *mut ResultHeader {
    let Some(layout) = result_layout(payload.len()) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the layout has a non-zero size (the header alone is 16 bytes).
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: the block holds RESULT_PAYLOAD_OFFSET + payload.len() bytes, so the payload area is
    // in bounds and cannot overlap `payload`, which the caller still owns.
    unsafe {
        let data = block.add(RESULT_PAYLOAD_OFFSET);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), data, payload.len());
        block.cast::<ResultHeader>().write(ResultHeader {
            ptr: data as usize as u32,
            len: payload.len() as u32,
            status,
        });
    }
    block.cast()
}

/// A successful result carrying `payload`.
pub fn ok_result(payload: &[u8]) -> *mut ResultHeader {
    result(STATUS_OK, payload)
}

/// A failed result: `status` is the `ErrorCode` number and `payload` the `ApiError` JSON. A
/// status of 0 would read as success, so it is refused.
pub fn err_result(status: u32, json: &str) -> *mut ResultHeader {
    debug_assert_ne!(status, STATUS_OK, "0 is STATUS_OK, not an error code");
    result(status, json.as_bytes())
}

/// The ABI version; the worker checks it at load.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_abi_version() -> u32 {
    ABI_VERSION
}

/// Allocates `len` bytes of linear memory for the host to fill, 8-byte aligned, or null when
/// `len` is 0 or the allocation failed.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_alloc(len: u32) -> *mut u8 {
    alloc_bytes(len as usize)
}

/// Frees memory from `pemu_alloc`. `len` must be the length that call was given.
///
/// # Safety
///
/// `ptr` must be null, or a live pointer from `pemu_alloc(len)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_free(ptr: *mut u8, len: u32) {
    // SAFETY: the caller promises ptr came from pemu_alloc with this len.
    unsafe { free_bytes(ptr, len as usize) }
}

/// Frees a result header and its payload. Null is ignored, so a worker may free unconditionally.
///
/// # Safety
///
/// `res` must be null, or a live pointer returned by an ABI call that returns `res`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_result_free(res: *mut ResultHeader) {
    if res.is_null() {
        return;
    }
    // SAFETY: the caller promises res came from one of the `res` calls, so it points at a block
    // `result` allocated with result_layout of exactly this payload length.
    unsafe {
        let len = (*res).len as usize;
        if let Some(layout) = result_layout(len) {
            dealloc(res.cast::<u8>(), layout);
        }
    }
}

/// What `pemu_run` answers for a handle that names no machine: no `StopCode` is this large.
pub const NO_MACHINE: u32 = u32::MAX;

/// The result of an `ApiError`: its code number as the status and its JSON as the payload.
fn api_err(error: &ApiError) -> *mut ResultHeader {
    err_result(error.status(), &error.to_json_text())
}

fn json_result(answer: Result<serde_json::Value, ApiError>) -> *mut ResultHeader {
    match answer {
        Ok(value) => ok_result(value.to_string().as_bytes()),
        Err(error) => api_err(&error),
    }
}

/// `len` bytes at `ptr` as a slice; an empty slice for a zero length, whatever the pointer.
///
/// # Safety
///
/// When `len` is not 0, `ptr` must point at `len` readable bytes that stay unchanged for the call.
unsafe fn bytes_at<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        // SAFETY: the caller promises `len` readable bytes at `ptr`.
        unsafe { core::slice::from_raw_parts(ptr, len as usize) }
    }
}

/// Starts a builder from a JSON configuration (`crate::config`). Never null: a configuration
/// that does not parse is kept and reported by `pemu_build`.
///
/// # Safety
///
/// `cfg_ptr` must point at `cfg_len` readable bytes, or `cfg_len` must be 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_new(cfg_ptr: *const u8, cfg_len: u32) -> *mut MachineBuilder {
    crate::commands::install();
    // SAFETY: the caller promises the configuration bytes.
    let cfg = unsafe { bytes_at(cfg_ptr, cfg_len) };
    Box::into_raw(Box::new(MachineBuilder::new(cfg)))
}

/// Loads one asset into a builder. `kind`: 0 ROM ELF (optional override), 1 merged flash or a
/// `.pebundle`, 2 app ELF, 3 bootloader ELF, 4 eFuse image (see `LoadKind`).
///
/// # Safety
///
/// `builder` must be a live pointer from `pemu_new` not yet passed to `pemu_build`; `ptr` must
/// point at `len` readable bytes, or `len` must be 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_load(
    builder: *mut MachineBuilder,
    kind: u32,
    ptr: *const u8,
    len: u32,
) -> *mut ResultHeader {
    if builder.is_null() {
        return api_err(&ApiError::new(E_USAGE, "`pemu_load` got no builder"));
    }
    // SAFETY: the caller promises a live builder and the asset bytes.
    let (builder, bytes) = unsafe { (&mut *builder, bytes_at(ptr, len)) };
    match builder.load(kind, bytes) {
        Ok(()) => ok_result(&[]),
        Err(error) => api_err(&error),
    }
}

/// Consumes the builder; payload = `u32` handle, little-endian. Without kind 0 it uses the bundled
/// ROM, and without kind 4 the synthesized eFuse.
///
/// # Safety
///
/// `builder` must be a live pointer from `pemu_new`; it is freed here whatever the outcome.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_build(builder: *mut MachineBuilder) -> *mut ResultHeader {
    if builder.is_null() {
        return api_err(&ApiError::new(E_USAGE, "`pemu_build` got no builder"));
    }
    // SAFETY: the caller promises a live builder from `pemu_new`, which `Box::into_raw` made.
    let builder = unsafe { Box::from_raw(builder) };
    match instance::build(*builder) {
        Ok(handle) => ok_result(&handle.to_le_bytes()),
        Err(error) => api_err(&error),
    }
}

/// Frees the machine, its rings and its layout, and ends its pool session. An unknown handle is
/// ignored.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_drop(handle: u32) {
    instance::with_table(|table| {
        table.drop_handle(handle);
    });
}

/// Runs the machine with the `@stops` armed; returns the stop code only
/// ([`crate::layout::stop_code`]), or [`NO_MACHINE`]. A negative limit is no limit.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_run(handle: u32, until_ps: i64, max_insns: i64) -> u32 {
    instance::with_table(|table| match table.get(handle) {
        Some(instance) => instance.run(until_ps, max_insns),
        None => NO_MACHINE,
    })
}

/// JSON `StopReason` with its payload and the `RunOutcome` fields of the last run;
/// `{"reason": null}` before the first run.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_last_stop(handle: u32) -> *mut ResultHeader {
    json_result(instance::with_table(|table| {
        table
            .get(handle)
            .map(|instance| instance.last_stop_json())
            .ok_or_else(|| instance::no_handle(handle))
    }))
}

/// Journals inputs: a JSON array of `{at, event}`, or fixed-layout records
/// ([`crate::input_batch`]). Payload `{"journaled": n}`.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes, or `len` must be 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_input(handle: u32, ptr: *const u8, len: u32) -> *mut ResultHeader {
    // SAFETY: the caller promises the input bytes.
    let bytes = unsafe { bytes_at(ptr, len) };
    json_result(instance::with_table(|table| match table.get(handle) {
        Some(instance) => instance.input(bytes),
        None => Err(instance::no_handle(handle)),
    }))
}

/// Offsets of every ring and the frame buffer, plus the layout generation counter; null for an
/// unknown handle.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_io_layout(handle: u32) -> *const IoLayout {
    instance::with_table(|table| match table.get(handle) {
        Some(instance) => instance.publisher().layout_ptr(),
        None => core::ptr::null(),
    })
}

/// The machine's virtual time in picoseconds, or -1 for an unknown handle.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_now_ps(handle: u32) -> i64 {
    instance::with_table(|table| match table.get(handle) {
        Some(instance) => i64::try_from(instance.machine().now().0).unwrap_or(i64::MAX),
        None => -1,
    })
}

/// Runs a registry command, `{"cmd", "args"}` in and the command output JSON out, or a reserved
/// `@` request; stops are armed here, never through `pemu_run`.
///
/// # Safety
///
/// `json_ptr` must point at `json_len` readable bytes, or `json_len` must be 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_call(
    handle: u32,
    json_ptr: *const u8,
    json_len: u32,
) -> *mut ResultHeader {
    // SAFETY: the caller promises the request bytes.
    let bytes = unsafe { bytes_at(json_ptr, json_len) };
    json_result(instance::with_table(|table| {
        instance::call(table, handle, bytes)
    }))
}

/// Takes a local snapshot; payload = snapshot bytes. `flags` must be 0.
#[unsafe(no_mangle)]
pub extern "C" fn pemu_snapshot(handle: u32, flags: u32) -> *mut ResultHeader {
    let answer = instance::with_table(|table| match table.get(handle) {
        Some(instance) => instance.snapshot(flags),
        None => Err(instance::no_handle(handle)),
    });
    match answer {
        Ok(bytes) => ok_result(&bytes),
        Err(error) => api_err(&error),
    }
}

/// Restores snapshot bytes. Rings are written in place, never reallocated; the view generation is
/// bumped so the worker re-creates its views.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes, or `len` must be 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pemu_restore(handle: u32, ptr: *const u8, len: u32) -> *mut ResultHeader {
    // SAFETY: the caller promises the snapshot bytes.
    let bytes = unsafe { bytes_at(ptr, len) };
    let answer = instance::with_table(|table| match table.get(handle) {
        Some(instance) => instance.restore(bytes),
        None => Err(instance::no_handle(handle)),
    });
    match answer {
        Ok(()) => ok_result(&[]),
        Err(error) => api_err(&error),
    }
}

#[cfg(test)]
mod machine_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_is_the_layout_constant() {
        assert_eq!(pemu_abi_version(), ABI_VERSION);
    }

    #[test]
    fn alloc_gives_aligned_writable_memory_that_free_takes_back() {
        let ptr = pemu_alloc(40);
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % ALLOC_ALIGN, 0);
        // SAFETY: pemu_alloc returned 40 writable bytes.
        unsafe {
            core::ptr::write_bytes(ptr, 0xA5, 40);
            assert_eq!(*ptr.add(39), 0xA5);
            pemu_free(ptr, 40);
        }
    }

    #[test]
    fn a_zero_length_allocation_is_null_and_freeing_null_is_safe() {
        assert!(pemu_alloc(0).is_null());
        // SAFETY: null is explicitly allowed by both functions.
        unsafe {
            pemu_free(core::ptr::null_mut(), 0);
            pemu_free(core::ptr::null_mut(), 16);
            pemu_result_free(core::ptr::null_mut());
        }
    }

    #[test]
    fn a_result_carries_its_payload_and_frees_it_whole() {
        let res = ok_result(b"{\"ok\":true}");
        assert!(!res.is_null());
        // SAFETY: res is the header ok_result just built, with its payload in the same block.
        unsafe {
            assert_eq!((*res).status, STATUS_OK);
            assert_eq!((*res).len, 11);
            let data = res.cast::<u8>().add(RESULT_PAYLOAD_OFFSET);
            assert_eq!(core::slice::from_raw_parts(data, 11), b"{\"ok\":true}");
            assert_eq!(
                (*res).ptr,
                (res as usize as u32).wrapping_add(RESULT_PAYLOAD_OFFSET as u32),
                "the published address is the payload's, exactly so on wasm32"
            );
            pemu_result_free(res);
        }
    }

    #[test]
    fn an_empty_result_has_no_payload_bytes_and_still_frees() {
        let res = ok_result(&[]);
        // SAFETY: res is the header ok_result just built.
        unsafe {
            assert_eq!((*res).len, 0);
            assert_ne!((*res).ptr, 0, "the address is past the header, never null");
            pemu_result_free(res);
        }
    }

    #[test]
    fn an_error_result_carries_the_error_code_as_its_status() {
        let res = err_result(41, "{\"code\":\"E_LEASE\"}");
        // SAFETY: res is the header err_result just built.
        unsafe {
            assert_eq!((*res).status, 41);
            assert_ne!((*res).status, STATUS_OK);
            pemu_result_free(res);
        }
    }
}
