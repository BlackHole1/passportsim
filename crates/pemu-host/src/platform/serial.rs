//! Serial enumeration without opening a port, behind feature `device`.
//!
//! Listing candidate devices is the only flash step that runs before a person confirms anything,
//! so it must touch nothing: opening a CDC port can block on carrier, a Windows open can disturb
//! the device, and esptool would put the app into download mode. Both hosts ask the OS registry
//! instead (IOKit on macOS, SetupAPI on Windows), and match on USB identifiers, never on a port
//! name, which any adapter can have. Any other host gets `E_HOST_UNSUPPORTED`.
//!
//! The feature pulls in no serial-port crate (only `pemu-planner` may have one); on Windows it
//! enables only the SetupAPI feature of `windows-sys`. `cargo xtask layering` checks both.

use std::path::PathBuf;

use pemu_api::error::{ApiError, E_HOST_UNSUPPORTED};

/// USB vendor id the enumeration matches on (Espressif).
pub const VID: u16 = 0x303A;
/// USB product id of the ESP32-C3 USB Serial/JTAG device.
pub const PID: u16 = 0x1001;

/// One serial device found without opening it. It carries only the identifiers matched on and the
/// path a later step would open: no serial number or product string, which identify a unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialDevice {
    /// The call-out device on macOS, the port name on Windows. Opened only after confirmation.
    pub path: PathBuf,
    pub vid: u16,
    pub pid: u16,
}

impl SerialDevice {
    pub fn is_passport_bridge(&self) -> bool {
        self.vid == VID && self.pid == PID
    }
}

/// Serial enumeration, one implementation per host.
pub trait SerialEnumeration: Send + Sync {
    /// Lists the serial devices with USB identifiers `(vid, pid)` without opening any: only the OS
    /// device registry is read, no `open`, no `stat` of the node, no reset line touched.
    fn enumerate(&self, vid: u16, pid: u16) -> Result<Vec<SerialDevice>, ApiError>;
}

pub fn enumerator() -> &'static dyn SerialEnumeration {
    #[cfg(target_os = "macos")]
    {
        &macos::IoKit
    }
    #[cfg(windows)]
    {
        &setupapi::SetupApi
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        &unsupported::NoEnumeration
    }
}

/// The Passport's USB Serial/JTAG bridge, and only it.
pub fn discover() -> Result<Vec<SerialDevice>, ApiError> {
    enumerator().enumerate(VID, PID)
}

/// `E_HOST_UNSUPPORTED` with a host-neutral alternative, for a host other than macOS or Windows.
/// Built outside the `cfg` arm so both supported hosts can test it.
pub fn host_unsupported() -> ApiError {
    ApiError::new(
        E_HOST_UNSUPPORTED,
        "serial enumeration runs on macOS (IOKit) and Windows (SetupAPI) only",
    )
    .with_hint(
        "run the planner with `--dry-run`, which is host-neutral, or flash from macOS or Windows",
    )
}

/// The USB vendor and product ids of one SetupAPI hardware id such as
/// `USB\VID_303A&PID_1001&REV_0101&MI_00` (Microsoft Learn, "Standard USB Identifiers"), or `None`.
/// Fields are split on `\` and `&` and matched exactly and case-insensitively, so `VID_303A0` or
/// `SUBVID_303A` is not a vendor id. A hardware id carries no serial number.
#[cfg_attr(not(windows), allow(dead_code))]
fn usb_ids(hardware_id: &str) -> Option<(u16, u16)> {
    let field = |key: &str| {
        hardware_id
            .split(['\\', '&'])
            .find_map(|part| {
                let (name, value) = part.split_at_checked(key.len())?;
                name.eq_ignore_ascii_case(key).then_some(value)
            })
            .filter(|hex| hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
    };
    Some((field("VID_")?, field("PID_")?))
}

/// The arm of a host that is neither macOS nor Windows.
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use super::{ApiError, SerialDevice, SerialEnumeration};

    pub struct NoEnumeration;

    impl SerialEnumeration for NoEnumeration {
        fn enumerate(&self, _vid: u16, _pid: u16) -> Result<Vec<SerialDevice>, ApiError> {
            Err(super::host_unsupported())
        }
    }
}

/// The IOKit backend.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, c_char, c_void};
    use std::path::PathBuf;

    use pemu_api::error::E_INTERNAL;

    use super::{ApiError, SerialDevice, SerialEnumeration};

    type IoObject = u32;
    type CfRef = *const c_void;

    /// `kIOMainPortDefault`: port name 0 means the default, which avoids the renamed
    /// `kIOMasterPortDefault` symbol.
    const MAIN_PORT_DEFAULT: u32 = 0;
    const KERN_SUCCESS: i32 = 0;
    const UTF8: u32 = 0x0800_0100;
    const CF_NUMBER_SINT32: isize = 3;
    /// `kIORegistryIterateRecursively | kIORegistryIterateParents`: the USB identifiers sit on an
    /// ancestor of the serial client.
    const SEARCH_PARENTS: u32 = 0x0000_0001 | 0x0000_0002;
    const PATH_MAX: usize = 1024;

    // SAFETY of every declaration below: documented IOKit and CoreFoundation entry points with
    // stable ABIs from the two linked system frameworks. Ownership is stated on each wrapper; no
    // Core Foundation object is held across a call and everything created or copied is released.
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        /// Builds the matching dictionary for an IOKit class. The reference is consumed by
        /// `IOServiceGetMatchingServices`.
        #[allow(non_snake_case)]
        fn IOServiceMatching(class: *const c_char) -> CfRef;
        #[allow(non_snake_case)]
        fn IOServiceGetMatchingServices(
            main_port: u32,
            matching: CfRef,
            iterator: *mut IoObject,
        ) -> i32;
        /// The next object of an iterator, or 0 at the end. The caller owns what it returns.
        #[allow(non_snake_case)]
        fn IOIteratorNext(iterator: IoObject) -> IoObject;
        #[allow(non_snake_case)]
        fn IOObjectRelease(object: IoObject) -> i32;
        /// Copies one property of a registry entry. The caller owns the result.
        #[allow(non_snake_case)]
        fn IORegistryEntryCreateCFProperty(
            entry: IoObject,
            key: CfRef,
            allocator: CfRef,
            options: u32,
        ) -> CfRef;
        /// Copies one property of a registry entry or its ancestors in `plane`. The caller owns
        /// the result.
        #[allow(non_snake_case)]
        fn IORegistryEntrySearchCFProperty(
            entry: IoObject,
            plane: *const c_char,
            key: CfRef,
            allocator: CfRef,
            options: u32,
        ) -> CfRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        /// Creates a `CFString` from a NUL-terminated C string. The caller owns the result.
        #[allow(non_snake_case)]
        fn CFStringCreateWithCString(allocator: CfRef, cstr: *const c_char, encoding: u32)
        -> CfRef;
        /// Copies a `CFString` into `buffer`, returning false when it does not fit.
        #[allow(non_snake_case)]
        fn CFStringGetCString(
            string: CfRef,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> u8;
        /// Reads a `CFNumber` into `value`, returning false when the conversion is lossy.
        #[allow(non_snake_case)]
        fn CFNumberGetValue(number: CfRef, number_type: isize, value: *mut c_void) -> u8;
        #[allow(non_snake_case)]
        fn CFGetTypeID(cf: CfRef) -> usize;
        #[allow(non_snake_case)]
        fn CFStringGetTypeID() -> usize;
        #[allow(non_snake_case)]
        fn CFNumberGetTypeID() -> usize;
        #[allow(non_snake_case)]
        fn CFRelease(cf: CfRef);
    }

    struct CfString(CfRef);

    impl CfString {
        fn new(text: &CStr) -> Option<CfString> {
            // SAFETY: `text` is NUL-terminated and outlives the call; the default allocator is
            // null, and the returned reference is owned here and released in `Drop`.
            let cf = unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) };
            // Not `then_some`: it would build an owning wrapper over null eagerly, and dropping it
            // would call `CFRelease(NULL)`, which traps.
            match cf.is_null() {
                true => None,
                false => Some(CfString(cf)),
            }
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: created by `CFStringCreateWithCString` above and released exactly once.
            unsafe { CFRelease(self.0) };
        }
    }

    struct CfProperty(CfRef);

    impl Drop for CfProperty {
        fn drop(&mut self) {
            // SAFETY: every constructor is a `Create`/`Copy` IOKit call whose result the caller
            // owns; released exactly once.
            unsafe { CFRelease(self.0) };
        }
    }

    impl CfProperty {
        fn as_string(&self) -> Option<String> {
            // SAFETY: `self.0` is a live Core Foundation object; its type is checked before it is
            // read as a string, and the buffer length is passed.
            unsafe {
                if CFGetTypeID(self.0) != CFStringGetTypeID() {
                    return None;
                }
                let mut buffer = [0 as c_char; PATH_MAX];
                if CFStringGetCString(self.0, buffer.as_mut_ptr(), PATH_MAX as isize, UTF8) == 0 {
                    return None;
                }
                CStr::from_ptr(buffer.as_ptr())
                    .to_str()
                    .ok()
                    .map(str::to_owned)
            }
        }

        /// The property as a USB identifier, or `None` when missing, not a `CFNumber`, or over
        /// 16 bits.
        fn as_u16(&self) -> Option<u16> {
            // SAFETY: the type is checked first, and the out-parameter is a live `i32`.
            unsafe {
                if CFGetTypeID(self.0) != CFNumberGetTypeID() {
                    return None;
                }
                let mut value: i32 = 0;
                let ok =
                    CFNumberGetValue(self.0, CF_NUMBER_SINT32, (&raw mut value).cast::<c_void>());
                (ok != 0)
                    .then_some(value)
                    .and_then(|v| u16::try_from(v).ok())
            }
        }
    }

    fn property(entry: IoObject, key: &CStr) -> Option<CfProperty> {
        let key = CfString::new(key)?;
        // SAFETY: `entry` is live and owned by the caller and `key` outlives the call; the result
        // is owned here.
        let value = unsafe { IORegistryEntryCreateCFProperty(entry, key.0, std::ptr::null(), 0) };
        match value.is_null() {
            true => None,
            false => Some(CfProperty(value)),
        }
    }

    fn ancestor_property(entry: IoObject, key: &CStr) -> Option<CfProperty> {
        let key = CfString::new(key)?;
        // SAFETY: as above; the plane name is a NUL-terminated literal and the search only reads.
        let value = unsafe {
            IORegistryEntrySearchCFProperty(
                entry,
                c"IOService".as_ptr(),
                key.0,
                std::ptr::null(),
                SEARCH_PARENTS,
            )
        };
        match value.is_null() {
            true => None,
            false => Some(CfProperty(value)),
        }
    }

    struct IoIterator(IoObject);

    impl Drop for IoIterator {
        fn drop(&mut self) {
            // SAFETY: obtained from `IOServiceGetMatchingServices` and released exactly once.
            unsafe { IOObjectRelease(self.0) };
        }
    }

    impl Iterator for IoIterator {
        type Item = IoService;

        fn next(&mut self) -> Option<IoService> {
            // SAFETY: `self.0` is a live iterator; the object it hands back is owned here.
            let object = unsafe { IOIteratorNext(self.0) };
            match object {
                0 => None,
                object => Some(IoService(object)),
            }
        }
    }

    struct IoService(IoObject);

    impl Drop for IoService {
        fn drop(&mut self) {
            // SAFETY: obtained from `IOIteratorNext` and released exactly once.
            unsafe { IOObjectRelease(self.0) };
        }
    }

    pub struct IoKit;

    impl SerialEnumeration for IoKit {
        /// Walks the `IOSerialBSDClient` services and keeps those whose USB ancestor carries
        /// `(vid, pid)`. The registry is read through a mach port, so nothing is opened. Only the
        /// call-out (`cu`) node is reported; the dial-in (`tty`) open blocks until carrier. A
        /// failing IOKit call is `E_INTERNAL`, not `E_HOST_UNSUPPORTED`, since macOS supports it.
        fn enumerate(&self, vid: u16, pid: u16) -> Result<Vec<SerialDevice>, ApiError> {
            // SAFETY: the matching dictionary for a literal class name is consumed by
            // `IOServiceGetMatchingServices`, so it is not released here; the iterator
            // out-parameter is a live `u32`.
            let iterator = unsafe {
                let matching = IOServiceMatching(c"IOSerialBSDClient".as_ptr());
                if matching.is_null() {
                    return Err(ApiError::new(
                        E_INTERNAL,
                        "IOKit refused to build the serial matching dictionary, so no port could be \
                         enumerated",
                    ));
                }
                let mut iterator: IoObject = 0;
                let status =
                    IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, matching, &raw mut iterator);
                if status != KERN_SUCCESS {
                    return Err(ApiError::new(
                        E_INTERNAL,
                        format!("IOKit refused the serial-device lookup (kern_return {status})"),
                    ));
                }
                IoIterator(iterator)
            };

            let mut found = Vec::new();
            for service in iterator {
                let Some(path) =
                    property(service.0, c"IOCalloutDevice").and_then(|p| p.as_string())
                else {
                    continue;
                };
                let ids = (
                    ancestor_property(service.0, c"idVendor").and_then(|p| p.as_u16()),
                    ancestor_property(service.0, c"idProduct").and_then(|p| p.as_u16()),
                );
                if ids == (Some(vid), Some(pid)) {
                    found.push(SerialDevice {
                        path: PathBuf::from(path),
                        vid,
                        pid,
                    });
                }
            }
            found.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(found)
        }
    }
}

/// The SetupAPI backend.
#[cfg(windows)]
mod setupapi {
    use std::path::PathBuf;

    use pemu_api::error::E_INTERNAL;
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV, GUID_DEVCLASS_PORTS, HDEVINFO, SP_DEVINFO_DATA,
        SPDRP_HARDWAREID, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
        SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey,
    };
    use windows_sys::Win32::Foundation::{
        ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, KEY_QUERY_VALUE, REG_MULTI_SZ, REG_SZ, RegCloseKey, RegQueryValueExW,
    };

    use super::{ApiError, SerialDevice, SerialEnumeration, usb_ids};

    /// The registry value under a port's device key holding its `COM<n>` name (Microsoft Learn,
    /// "Ports Class").
    const PORT_NAME: &str = "PortName";

    struct DeviceSet(HDEVINFO);

    impl Drop for DeviceSet {
        fn drop(&mut self) {
            // SAFETY: returned by `SetupDiGetClassDevsW` and destroyed exactly once.
            unsafe { SetupDiDestroyDeviceInfoList(self.0) };
        }
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: returned open by `SetupDiOpenDevRegKey` and closed exactly once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    /// NUL-separated UTF-16 strings, as a `REG_MULTI_SZ` or `REG_SZ` value holds them.
    fn strings(units: &[u16]) -> Vec<String> {
        units
            .split(|&u| u == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }

    /// The hardware ids of one device (`SPDRP_HARDWAREID`, a `REG_MULTI_SZ`), or `None`.
    fn hardware_ids(set: &DeviceSet, device: &SP_DEVINFO_DATA) -> Option<Vec<String>> {
        let mut kind = 0u32;
        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; the call writes the size it needs.
        let sized = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                device,
                SPDRP_HARDWAREID,
                &raw mut kind,
                std::ptr::null_mut(),
                0,
                &raw mut needed,
            )
        };
        // SAFETY: reads the calling thread's last error, set by the call above.
        if sized != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
            return None;
        }
        let mut buffer = vec![0u16; (needed as usize).div_ceil(2) + 1];
        let bytes = u32::try_from(buffer.len() * 2).ok()?;
        // SAFETY: the buffer is `bytes` long and live for the call.
        let ok = unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                device,
                SPDRP_HARDWAREID,
                &raw mut kind,
                buffer.as_mut_ptr().cast(),
                bytes,
                &raw mut needed,
            )
        };
        (ok != 0 && kind == REG_MULTI_SZ).then(|| strings(&buffer))
    }

    /// The `PortName` of one device, from its hardware registry key (`DIREG_DEV`). This opens a
    /// registry key with query access only, never the device.
    fn port_name(set: &DeviceSet, device: &SP_DEVINFO_DATA) -> Option<String> {
        // SAFETY: the set and the device data are live; the key is closed by `Key`.
        let key = unsafe {
            SetupDiOpenDevRegKey(
                set.0,
                device,
                DICS_FLAG_GLOBAL,
                0,
                DIREG_DEV,
                KEY_QUERY_VALUE,
            )
        };
        if key.is_null() || key == INVALID_HANDLE_VALUE {
            return None;
        }
        let key = Key(key);
        let name: Vec<u16> = PORT_NAME.encode_utf16().chain([0]).collect();
        let mut kind = 0u32;
        let mut size = 0u32;
        // SAFETY: a size query; `name` is NUL-terminated.
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &raw mut kind,
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        if status != 0 || kind != REG_SZ || size == 0 {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut bytes = u32::try_from(buffer.len() * 2).ok()?;
        // SAFETY: the buffer is `bytes` long and live for the call.
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                std::ptr::null(),
                &raw mut kind,
                buffer.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        };
        if status != 0 || kind != REG_SZ {
            return None;
        }
        strings(&buffer).into_iter().next()
    }

    pub struct SetupApi;

    impl SerialEnumeration for SetupApi {
        /// Walks the present devices of the Ports setup class (`GUID_DEVCLASS_PORTS`) and keeps
        /// those whose hardware id carries `(vid, pid)`, reported by `PortName`. Nothing is
        /// opened. The instance id, whose suffix can carry the USB serial number and with it the
        /// MAC, is never asked for. A failing SetupAPI call is `E_INTERNAL`.
        fn enumerate(&self, vid: u16, pid: u16) -> Result<Vec<SerialDevice>, ApiError> {
            // SAFETY: a documented class GUID constant, no enumerator string and no window; the
            // set is destroyed by `DeviceSet`.
            let set = unsafe {
                SetupDiGetClassDevsW(
                    &GUID_DEVCLASS_PORTS,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    DIGCF_PRESENT,
                )
            };
            if set == INVALID_HANDLE_VALUE as HDEVINFO {
                // SAFETY: reads the last error the call above set.
                let error = unsafe { GetLastError() };
                return Err(ApiError::new(
                    E_INTERNAL,
                    format!(
                        "SetupAPI refused the Ports-class lookup (Windows error {error}), so no port could be \
                         enumerated"
                    ),
                ));
            }
            let set = DeviceSet(set);
            let mut found = Vec::new();
            for index in 0u32.. {
                let mut device = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..SP_DEVINFO_DATA::default()
                };
                // SAFETY: the set is live and the out-structure has its size set, as required.
                if unsafe { SetupDiEnumDeviceInfo(set.0, index, &raw mut device) } == 0 {
                    // SAFETY: reads the last error the call above set.
                    let error = unsafe { GetLastError() };
                    if error == ERROR_NO_MORE_ITEMS {
                        break;
                    }
                    return Err(ApiError::new(
                        E_INTERNAL,
                        format!("SetupAPI failed while listing ports (Windows error {error})"),
                    ));
                }
                let Some(ids) = hardware_ids(&set, &device) else {
                    continue;
                };
                if !ids.iter().any(|id| usb_ids(id) == Some((vid, pid))) {
                    continue;
                }
                if let Some(name) = port_name(&set, &device) {
                    found.push(SerialDevice {
                        path: PathBuf::from(name),
                        vid,
                        pid,
                    });
                }
            }
            found.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(found)
        }
    }
    // End of the SetupAPI arm.
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_SUCH_DEVICE: (u16, u16) = (0xFFFF, 0xFFFF);

    /// Every serial device this process has open, by the path behind each descriptor. Only the
    /// two serial spellings are collected: parallel spawn tests open and close null-device
    /// descriptors while this samples, and no test here opens a serial device.
    #[cfg(target_os = "macos")]
    fn open_serial_devices() -> Vec<String> {
        use std::ffi::{CStr, c_char};

        /// `F_GETPATH`: writes the path behind a descriptor into a `MAXPATHLEN` buffer, opening
        /// nothing.
        const F_GETPATH: i32 = 50;
        const MAXPATHLEN: usize = 1024;

        // SAFETY: `fcntl` is a libSystem entry point; `F_GETPATH` takes one buffer of at least
        // `MAXPATHLEN` bytes, which is what is passed, and writes a NUL-terminated path.
        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        }

        let mut devices: Vec<String> = std::fs::read_dir(concat!("/dev/", "fd"))
            .expect("the per-process descriptor directory")
            .filter_map(|entry| {
                let fd: i32 = entry.ok()?.file_name().to_string_lossy().parse().ok()?;
                let mut buffer = [0 as c_char; MAXPATHLEN];
                // SAFETY: see the declaration; the buffer is `MAXPATHLEN` bytes and live here.
                let ok = unsafe { fcntl(fd, F_GETPATH, buffer.as_mut_ptr()) } == 0;
                // SAFETY: on success the call wrote a NUL-terminated path into the buffer.
                let path = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
                let serial = path.starts_with(crate::paths::DEV_CU)
                    || path.starts_with(crate::paths::DEV_TTY);
                (ok && serial).then(|| path.into_owned())
            })
            .collect();
        devices.sort();
        devices
    }

    /// Two assertions: the set of open devices is unchanged, and no serial device is open at all.
    #[cfg(target_os = "macos")]
    #[test]
    fn enumeration_opens_nothing() {
        let before = open_serial_devices();
        let found = discover().expect("IOKit enumeration");
        let after = open_serial_devices();
        assert_eq!(
            before, after,
            "enumeration goes through the IO registry and opens no serial device"
        );
        assert!(
            after.is_empty(),
            "no serial device is open anywhere in this process: {after:?}"
        );
        for device in &found {
            assert!(device.is_passport_bridge(), "{device:?}");
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn matching_is_on_the_identifiers() {
        let found = enumerator()
            .enumerate(NO_SUCH_DEVICE.0, NO_SUCH_DEVICE.1)
            .expect("enumeration");
        assert!(found.is_empty(), "no device carries {NO_SUCH_DEVICE:04x?}");
    }

    /// Only the call-out node is a candidate; the dial-in node blocks until carrier. Silent when no
    /// Passport is plugged in, as in CI.
    #[cfg(target_os = "macos")]
    #[test]
    fn only_call_out_nodes_are_reported() {
        for device in discover().expect("IOKit enumeration") {
            let path = device.path.to_string_lossy().into_owned();
            assert!(
                path.starts_with(crate::paths::DEV_CU),
                "a candidate is a call-out node"
            );
            assert!(
                !path.starts_with(crate::paths::DEV_TTY),
                "the dial-in node is refused"
            );
            crate::paths::refuse_device(&device.path)
                .expect_err("an enumerated port is exactly what the serial-device guard refuses");
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn enumeration_is_repeatable() {
        assert_eq!(discover().expect("first"), discover().expect("second"));
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn a_third_host_refuses_instead_of_enumerating() {
        let e = discover().expect_err("no enumeration off macOS");
        assert_eq!(e.code, E_HOST_UNSUPPORTED, "{e}");
        let e = enumerator()
            .enumerate(NO_SUCH_DEVICE.0, NO_SUCH_DEVICE.1)
            .expect_err("the trait refuses too, whatever it is asked for");
        assert_eq!(e.code, E_HOST_UNSUPPORTED, "{e}");
    }

    #[test]
    fn the_identifiers_are_the_usb_serial_jtag_pair() {
        assert_eq!((VID, PID), (0x303A, 0x1001));
        let device = SerialDevice {
            path: PathBuf::from("x"),
            vid: VID,
            pid: PID,
        };
        assert!(device.is_passport_bridge());
        assert!(
            !SerialDevice {
                pid: 0x1002,
                ..device
            }
            .is_passport_bridge()
        );
    }

    /// Asserted on both supported hosts because neither runs the third-host arm.
    #[test]
    fn a_third_host_is_host_unsupported() {
        let e = host_unsupported();
        assert_eq!(e.code, E_HOST_UNSUPPORTED);
        assert!(e.message.contains("SetupAPI"), "{e}");
        assert!(e.hint.is_some(), "the error names what to do instead: {e}");
    }

    #[test]
    fn a_hardware_id_yields_its_usb_ids_and_nothing_else() {
        assert_eq!(
            usb_ids(r"USB\VID_303A&PID_1001&REV_0101&MI_00"),
            Some((0x303A, 0x1001))
        );
        assert_eq!(usb_ids(r"USB\VID_303A&PID_1001&MI_00"), Some((VID, PID)));
        assert_eq!(usb_ids(r"usb\vid_303a&pid_1001"), Some((VID, PID)));
        for other in [
            r"USB\VID_303A",
            r"USB\PID_1001",
            r"USB\VID_303A0&PID_1001",
            r"USB\VID_303&PID_1001",
            r"USB\SUBVID_303A&PID_1001",
            r"ACPI\PNP0501",
            r"FTDIBUS\COMPORT&VID_0403&PID_6001X",
            "",
        ] {
            assert_ne!(usb_ids(other), Some((VID, PID)), "{other}");
        }
        assert_eq!(usb_ids(r"ACPI\PNP0501"), None);
    }

    /// Two assertions: the SetupAPI arm's source names no entry point that opens a file or device
    /// or talks to a port, and a child process that only enumerates keeps its handle count (a
    /// child, because the rest of this suite opens handles in parallel).
    #[cfg(windows)]
    #[test]
    fn enumeration_opens_nothing() {
        let source = include_str!("serial.rs");
        let start = source
            .find(concat!("mod set", "upapi {"))
            .expect("the SetupAPI arm");
        let end = source
            .find(concat!("// End of the Set", "upAPI arm."))
            .expect("its end marker");
        let arm = &source[start..end];
        for call in [
            concat!("Create", "File"),
            concat!("NtCreate", "File"),
            concat!("Open", "Options"),
            concat!("File::", "open"),
            concat!("File::", "create"),
            concat!("Escape", "Comm"),
            concat!("Comm", "State"),
            concat!("Read", "File"),
            concat!("Write", "File"),
            concat!("Device", "IoControl"),
        ] {
            assert!(!arm.contains(call), "the SetupAPI arm names `{call}`");
        }

        let output = std::process::Command::new(std::env::current_exe().expect("this test"))
            .args([
                "platform::serial::tests::handle_count_probe",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .expect("the probe child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("handles unchanged"), "{stdout}");
    }

    /// The child of `enumeration_opens_nothing`: enumerates once so SetupAPI caches what it keeps,
    /// then checks three more leave the handle count unchanged. Prints counts only.
    #[cfg(windows)]
    #[test]
    #[ignore = "run by enumeration_opens_nothing in a process of its own"]
    fn handle_count_probe() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

        let count = || {
            let mut n = 0u32;
            // SAFETY: the pseudo-handle of this process and a live out-parameter.
            let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &raw mut n) };
            assert_ne!(ok, 0, "GetProcessHandleCount");
            n
        };
        discover().expect("the warm-up enumeration");
        let before = count();
        for _ in 0..3 {
            discover().expect("SetupAPI enumeration");
        }
        let after = count();
        assert_eq!(before, after, "an enumeration left a handle open");
        println!("handles unchanged: {before}");
    }

    /// With the Passport attached, exactly one port carries 303A:1001, and it is a `COM<n>` name
    /// the planner accepts. Prints the count only. Runs only when `PEMU_PASSPORT_ATTACHED` is set.
    #[cfg(windows)]
    #[test]
    fn the_attached_passport_is_found_exactly_once() {
        use pemu_planner::flow::DevicePaths as _;

        if std::env::var_os("PEMU_PASSPORT_ATTACHED").is_none() {
            println!("SKIP: PEMU_PASSPORT_ATTACHED is not set");
            return;
        }
        let found = discover().expect("SetupAPI enumeration");
        assert_eq!(found.len(), 1, "{} ports carry 303A:1001", found.len());
        let port = found[0].path.to_string_lossy().into_owned();
        assert!(
            pemu_planner::exec::ComPorts.is_flash_port(&port),
            "the port name is a COM<n> name"
        );
        assert!(found[0].is_passport_bridge());
        println!("found {} port with 303A:1001", found.len());
    }
}
