//! Windows process and thread services: the detached daemon spawn, the console control events a
//! foreground `serve` stops on, and the thread class of a machine thread. Every Win32 behavior a
//! choice depends on is cited from Microsoft Learn next to the choice.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::os::windows::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, FALSE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, TRUE,
    WAIT_FAILED,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    GetConsoleProcessList, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_UNICODE_ENVIRONMENT,
    CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentThread, GetExitCodeProcess, GetThreadInformation, INFINITE,
    InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, SetThreadInformation,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, TerminateProcess, ThreadPowerThrottling,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::core::BOOL;

use super::Windows;
use crate::platform::{
    DaemonSpawn, DetachedChild, PlatformError, QosClass, ShutdownFlag, ShutdownSignal, Signals,
    ThreadQos,
};

// -------------------------------------------------------------------------------------------------
// The detached daemon spawn
// -------------------------------------------------------------------------------------------------

/// `DETACHED_PROCESS`: "the new process does not inherit its parent's console", so closing the
/// starting terminal sends it no `CTRL_CLOSE_EVENT`. `CREATE_NEW_PROCESS_GROUP`: a Ctrl-C sent to
/// the starter's group does not reach it (Microsoft Learn, "Process Creation Flags").
/// `CREATE_NO_WINDOW` "is ignored if used with `DETACHED_PROCESS`". `EXTENDED_STARTUPINFO_PRESENT`
/// carries the handle list below.
const DETACHED_FLAGS: u32 =
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT;

/// The three null-device stdio handles, the only handles the daemon inherits.
///
/// `Command::spawn` sets `bInheritHandles`, so **every** inheritable handle of the parent reaches
/// the child: an MCP client's stdio pipes, or a shell's own handles. Measured: a daemon spawned
/// that way from a remote PowerShell session held the session's output pipe, and the session
/// could not end until the daemon was killed. So the spawn is a raw `CreateProcessW` with a
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming these three handles and nothing else
/// (Microsoft Learn, "UpdateProcThreadAttribute function").
struct NullStdio([OwnedHandle; 3]);

impl NullStdio {
    /// Opens the null device three times, inheritable, as the list requires. Another thread's
    /// spawn meanwhile can inherit one, which hands it a null device and nothing else.
    fn open() -> io::Result<NullStdio> {
        let name: Vec<u16> = "NUL".encode_utf16().chain([0]).collect();
        let inheritable = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let open = || -> io::Result<OwnedHandle> {
            // SAFETY: a NUL-terminated name, a security attributes structure this function owns,
            // and no template file.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &inheritable,
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a handle this function just opened and owns alone.
            Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
        };
        Ok(NullStdio([open()?, open()?, open()?]))
    }

    fn raw(&self) -> [HANDLE; 3] {
        self.0.each_ref().map(|h| h.as_raw_handle())
    }
}

struct AttributeList {
    buffer: Vec<u64>,
}

impl AttributeList {
    /// A list whose `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` is `handles`, which must outlive it.
    fn with_handle_list(handles: &[HANDLE; 3]) -> io::Result<AttributeList> {
        let mut size = 0usize;
        // SAFETY: the documented size query: a null list and a size out parameter.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size) };
        let mut list = AttributeList {
            buffer: vec![0u64; size.div_ceil(8)],
        };
        // SAFETY: a buffer of at least `size` bytes, 8-byte aligned.
        if unsafe { InitializeProcThreadAttributeList(list.ptr(), 1, 0, &mut size) } == 0 {
            // Nothing was initialized, so nothing may be deleted.
            let error = io::Error::last_os_error();
            std::mem::forget(list);
            return Err(error);
        }
        // SAFETY: an initialized list, and a value that outlives it (the caller's contract).
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of::<[HANDLE; 3]>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(list)
    }

    fn ptr(&mut self) -> *mut core::ffi::c_void {
        self.buffer.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: a list `with_handle_list` initialized; it is deleted once.
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

/// `arg` appended so the child's C runtime parses it back unchanged (Microsoft Learn, "Parsing C
/// command-line arguments"): backslashes are literal unless they precede a double quote, where
/// `2n` give `n` and the quote delimits, and `2n + 1` give `n` and a literal quote.
fn append_argument(line: &mut Vec<u16>, arg: &OsStr) {
    let units: Vec<u16> = arg.encode_wide().collect();
    let plain = !units.is_empty()
        && !units
            .iter()
            .any(|&u| u == u16::from(b' ') || u == u16::from(b'\t') || u == u16::from(b'"'));
    if plain {
        line.extend(units);
        return;
    }
    line.push(u16::from(b'"'));
    let mut backslashes = 0usize;
    for &unit in &units {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if unit == u16::from(b'"') {
            // Double the run and escape the quote itself.
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        backslashes = 0;
        line.push(unit);
    }
    // A run before the closing quote is doubled, so the quote still closes.
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    line.push(u16::from(b'"'));
}

/// The command line: the program, always quoted (it is parsed without backslash escapes, and a
/// path holds no double quote), then each argument.
fn command_line(command: &Command) -> Vec<u16> {
    let mut line = vec![u16::from(b'"')];
    line.extend(command.get_program().encode_wide());
    line.push(u16::from(b'"'));
    for arg in command.get_args() {
        line.push(u16::from(b' '));
        append_argument(&mut line, arg);
    }
    line.push(0);
    line
}

/// The environment block of `command`, or `None` when the child inherits this environment as is.
/// Names compare case-insensitively and are sorted that way, double-NUL terminated (Microsoft
/// Learn, "Changing Environment Variables").
fn environment_block(command: &Command) -> Option<Vec<u16>> {
    let changes: Vec<(OsString, Option<OsString>)> = command
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
        .collect();
    if changes.is_empty() {
        return None;
    }
    let upper = |key: &OsStr| key.to_string_lossy().to_uppercase();
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (key, value) in changes {
        vars.retain(|(k, _)| upper(k) != upper(&key));
        if let Some(value) = value {
            vars.push((key, value));
        }
    }
    vars.sort_by_key(|(k, _)| upper(k));
    let mut block = Vec::new();
    for (key, value) in vars {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    Some(block)
}

fn create_process(command: &Command, flags: u32) -> io::Result<DetachedChild> {
    let stdio = NullStdio::open()?;
    let handles = stdio.raw();
    let mut list = AttributeList::with_handle_list(&handles)?;
    // SAFETY: an all-zero `STARTUPINFOEXW` is its documented empty state; the fields below fill it.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = list.ptr();

    let program: Vec<u16> = command.get_program().encode_wide().chain([0]).collect();
    // An absolute program is named exactly; anything else is found by the documented search of
    // the command line's first token.
    let application = match std::path::Path::new(command.get_program()).is_absolute() {
        true => program.as_ptr(),
        false => std::ptr::null(),
    };
    let mut line = command_line(command);
    let environment = environment_block(command);
    let directory: Option<Vec<u16>> = command
        .get_current_dir()
        .map(|d| d.as_os_str().encode_wide().chain([0]).collect());
    let flags = match environment {
        Some(_) => flags | CREATE_UNICODE_ENVIRONMENT,
        None => flags,
    };
    // SAFETY: a zeroed out parameter this function owns.
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer is to a buffer this function owns that outlives the call; the
    // command line is mutable, as `CreateProcessW` may write into it. The handle list names only
    // the three null-device handles, so `bInheritHandles` passes those and nothing else.
    let ok = unsafe {
        CreateProcessW(
            application,
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            TRUE,
            flags,
            environment
                .as_ref()
                .map_or(std::ptr::null(), |block| block.as_ptr().cast()),
            directory
                .as_ref()
                .map_or(std::ptr::null(), |dir| dir.as_ptr()),
            (&raw const startup).cast(),
            &mut info,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles were just returned by `CreateProcessW`; the thread handle is dropped.
    let (process, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(info.hProcess),
            OwnedHandle::from_raw_handle(info.hThread),
        )
    };
    drop(list);
    drop(stdio);
    Ok(DetachedChild::from_handle(info.dwProcessId, process))
}

pub(crate) fn wait(process: &OwnedHandle) -> io::Result<std::process::ExitStatus> {
    // SAFETY: a live process handle this process owns.
    if unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } == WAIT_FAILED {
        return Err(io::Error::last_os_error());
    }
    let mut code = 0u32;
    // SAFETY: the same handle and an out parameter this function owns.
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(std::process::ExitStatus::from_raw(code))
}

pub(crate) fn kill(process: &OwnedHandle) -> io::Result<()> {
    // SAFETY: a live process handle this process owns.
    match unsafe { TerminateProcess(process.as_raw_handle(), 1) } {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}

impl DaemonSpawn for Windows {
    /// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, first with `CREATE_BREAKAWAY_FROM_JOB` so a
    /// kill-on-close job (an agent harness, a CI runner) does not take the daemon down with it.
    /// The job must allow breakaway (Microsoft Learn, "Process Creation Flags"), so
    /// `ERROR_ACCESS_DENIED`, and only it, is retried without the flag; the daemon then ends with
    /// its parent's job.
    ///
    /// The command's program, arguments, environment changes and working directory are used; its
    /// stdio and an `env_clear` are not. Never a plain `Command::spawn` fallback, whose child
    /// would inherit whatever its parent holds.
    fn spawn_detached(&self, command: &mut Command) -> Result<DetachedChild, PlatformError> {
        let failed = |e| PlatformError::io(PathBuf::new(), e);
        match create_process(command, DETACHED_FLAGS | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(child) => Ok(child),
            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                create_process(command, DETACHED_FLAGS).map_err(failed)
            }
            Err(e) => Err(failed(e)),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// The console control events of a foreground `serve`
// -------------------------------------------------------------------------------------------------

/// There is no `SIGTERM` on Windows, so the portable stop is an authenticated
/// `POST /v1/shutdown` and this list is the console control events.
const WINDOWS_SHUTDOWN_SIGNALS: &[ShutdownSignal] = &[
    ShutdownSignal::Interrupt,
    ShutdownSignal::Break,
    ShutdownSignal::Close,
    ShutdownSignal::SystemShutdown,
];

/// The flag the console control handler sets. The handler runs on a system-created thread, so it
/// stores to one atomic and the daemon's own thread does the flushing.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// How long the handler of an event that ends the process keeps its thread.
///
/// For close, logoff and shutdown events, returning `TRUE` makes the system terminate the process
/// (Microsoft Learn, "HandlerRoutine callback function"), before any artifact is flushed. Holding
/// the handler leaves the daemon's thread the time the system allows (5 s for a closed console),
/// and `serve` ends the process itself when the flush is done. The hold is longer than every
/// system timeout, so the system's deadline is what cuts a flush short.
const HOLD_FOR_THE_FLUSH: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    /// Not a shutdown: pass it on (`FALSE`), so the next handler or the default one sees it.
    PassOn,
    /// Ctrl-C or Ctrl-Break: set the flag and return; `serve` leaves through its ordinary path.
    Stop,
    /// Console closing or session ending: set the flag and hold the handler thread, because the
    /// system ends the process as soon as the handler returns.
    StopAndHold,
}

/// The handler's decision, apart from the `extern` function so it is tested without raising an
/// event that would end the test process.
fn control(event: u32) -> Control {
    match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => Control::Stop,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => Control::StopAndHold,
        _ => Control::PassOn,
    }
}

unsafe extern "system" fn console_control_handler(event: u32) -> BOOL {
    match control(event) {
        Control::PassOn => FALSE,
        Control::Stop => {
            SHUTDOWN.store(true, Ordering::SeqCst);
            TRUE
        }
        Control::StopAndHold => {
            SHUTDOWN.store(true, Ordering::SeqCst);
            std::thread::sleep(HOLD_FOR_THE_FLUSH);
            TRUE
        }
    }
}

/// Whether this process is attached to a console. `GetConsoleProcessList` fails with no console,
/// and any attached process counts itself, so a success is never zero.
fn has_console() -> bool {
    let mut one = [0u32; 1];
    // SAFETY: the buffer holds the one element the call is told it holds.
    unsafe { GetConsoleProcessList(one.as_mut_ptr(), 1) != 0 }
}

impl Signals for Windows {
    fn shutdown_signals(&self) -> &'static [ShutdownSignal] {
        WINDOWS_SHUTDOWN_SIGNALS
    }

    /// Installs [`console_control_handler`] for Ctrl-C, Ctrl-Break, console close and session end.
    ///
    /// **A process with no console refuses**: a detached daemon receives no control event and
    /// stops over `serve --stop` instead.
    ///
    /// **Ctrl-C is turned back on.** A `CREATE_NEW_PROCESS_GROUP` process inherits "ignore
    /// Ctrl-C", which `SetConsoleCtrlHandler(NULL, FALSE)` clears.
    ///
    /// **What does not arrive.** Logoff and shutdown events reach services only, and never a
    /// console process that loads `user32.dll` (the known-folder call in `paths` does, through
    /// `shell32`). [`ShutdownSignal::SystemShutdown`] is listed because the handler honors it;
    /// UNVERIFIED whether a session ending under a foreground `serve` ever delivers it.
    fn install_shutdown(&self) -> Result<ShutdownFlag, PlatformError> {
        if !has_console() {
            return Err(PlatformError::io(
                PathBuf::new(),
                io::Error::other(
                    "this process has no console, so no console control event can reach it \
                     (a detached daemon); `serve --stop` is its stop",
                ),
            ));
        }
        // SAFETY: a null handler with `FALSE` only clears this process's ignore-Ctrl-C attribute.
        unsafe { SetConsoleCtrlHandler(None, FALSE) };
        // SAFETY: `console_control_handler` has the `PHANDLER_ROUTINE` signature and touches only
        // an atomic and its own thread.
        if unsafe { SetConsoleCtrlHandler(Some(console_control_handler), TRUE) } == 0 {
            return Err(PlatformError::io(
                PathBuf::new(),
                io::Error::last_os_error(),
            ));
        }
        Ok(ShutdownFlag::from_static(&SHUTDOWN))
    }
}

// -------------------------------------------------------------------------------------------------
// The thread class of a machine thread
// -------------------------------------------------------------------------------------------------

/// The `ThreadPowerThrottling` state of the calling thread.
fn power_throttling() -> io::Result<THREAD_POWER_THROTTLING_STATE> {
    let mut state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    // SAFETY: the pseudo handle of the calling thread, and a buffer of exactly the size given.
    let ok = unsafe {
        GetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw mut state).cast(),
            size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        )
    };
    match ok {
        0 => Err(io::Error::last_os_error()),
        _ => Ok(state),
    }
}

/// The class a `ThreadPowerThrottling` state names, in the macOS vocabulary of [`QosClass`]
/// (Microsoft Learn, "Quality of Service"): execution-speed control set and state clear is
/// HighQoS, nearest to `QOS_CLASS_USER_INTERACTIVE`; both set is EcoQoS, which schedules to
/// efficient cores like `QOS_CLASS_BACKGROUND`. A thread controlling nothing reads
/// [`QosClass::Default`].
fn class_of(state: THREAD_POWER_THROTTLING_STATE) -> QosClass {
    let speed = THREAD_POWER_THROTTLING_EXECUTION_SPEED;
    match (state.ControlMask & speed != 0, state.StateMask & speed != 0) {
        (false, _) => QosClass::Default,
        (true, false) => QosClass::UserInteractive,
        (true, true) => QosClass::Background,
    }
}

impl ThreadQos for Windows {
    /// Tags the calling thread HighQoS (`SetThreadInformation(ThreadPowerThrottling)` with the
    /// execution-speed feature controlled and off), then reads it back.
    ///
    /// Measured by `thread_class_placement` (release, i7-12700F, no foreground window, as for the
    /// detached daemon): a 1 ms paced loop with about 60 us of work, beside 0, 8 and 20 busy
    /// threads:
    ///
    /// | Request | Bursts on class-0 cores | Burst p50 | Periods overrun |
    /// |---|---|---|---|
    /// | nothing | 100 % in all nine runs | 82 us | 0.2 to 50 % |
    /// | `THREAD_PRIORITY_ABOVE_NORMAL` | 100 % in all nine | 82 us | 0 to 2.2 % |
    /// | `THREAD_PRIORITY_HIGHEST` | 0 to 3.2 % | 62 us | 0 to 0.07 % |
    /// | HighQoS (this call) | 0 to 1.3 % | 62 us | 0 in eight, 5.5 % in one |
    /// | HighQoS and `HIGHEST` | 0 to 1.9 % | 62 us | 0 to 0.07 % |
    /// | EcoQoS | 100 % in all nine | 82 us | 1.5 to 50 % |
    ///
    /// HighQoS places the thread as well as `HIGHEST` without raising it above every other normal
    /// thread of the user, so the priority is left alone. The read-back reports the tag, not the
    /// core.
    fn request_interactive(&self) -> Result<QosClass, PlatformError> {
        let high = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        // SAFETY: the pseudo handle of the calling thread and a structure of exactly the size
        // given; only this thread's power-throttling state changes.
        let ok = unsafe {
            SetThreadInformation(
                GetCurrentThread(),
                ThreadPowerThrottling,
                (&raw const high).cast(),
                size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if ok == 0 {
            return Err(PlatformError::io(
                PathBuf::new(),
                io::Error::last_os_error(),
            ));
        }
        self.current()
    }

    fn current(&self) -> Result<QosClass, PlatformError> {
        power_throttling()
            .map(class_of)
            .map_err(|e| PlatformError::io(PathBuf::new(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::scratch_dir;
    use std::os::windows::process::CommandExt as _;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Instant;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    /// Polls `done` for up to 20 s.
    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    /// The name libtest filters on to run [`helper`] alone in a fresh process.
    const HELPER: &str = "platform::windows::process::tests::helper";
    const ROLE: &str = "PEMU_TEST_WIN_PROCESS_ROLE";
    const REPORT: &str = "PEMU_TEST_WIN_PROCESS_REPORT";

    fn helper_command(role: &str, report: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("this test binary"));
        command
            .args(["--exact", HELPER, "--test-threads", "1"])
            .env(ROLE, role)
            .env(REPORT, report);
        command
    }

    fn append(path: &Path, line: &str) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("the report file");
        writeln!(file, "{line}").expect("report");
    }

    fn marker(report: &Path) -> PathBuf {
        report.with_extension("alive")
    }

    fn appender(report: &Path) -> Command {
        helper_command("append", report)
    }

    /// **Not an assertion of its own.** The second process the tests below need, in the role its
    /// environment names; with [`ROLE`] unset it does nothing.
    ///
    /// - `append`: appends to the marker every 50 ms for about 3 s, then exits.
    /// - `outlive`: spawns a detached `append` and returns at once.
    /// - `console-of <pid>`: with no console, attaches to the console of `pid` and reports
    ///   `attached`, or `no-console` for `ERROR_INVALID_HANDLE`.
    /// - `ctrl`: alone in its own console, installs the handler and raises Ctrl-C and then
    ///   Ctrl-Break, reporting whether each reached the flag.
    /// - `close`: alone in its own console with a window, installs the handler, posts `WM_CLOSE`
    ///   to that window, reports whether the close reached the flag, and exits after a second of
    ///   "flush" while the handler holds. A window that is not a classic console window is
    ///   reported as `window-class <name>` instead, and nothing is posted.
    /// - `job <breakaway-ok|no-breakaway>`: joins a kill-on-close job, spawns a detached `append`,
    ///   reports whether that child is in the job, and exits, which closes the job.
    #[test]
    fn helper() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        let report = PathBuf::from(std::env::var_os(REPORT).expect("a helper has a report file"));
        let mut words = role.split_whitespace();
        match words.next() {
            Some("append") => {
                for _ in 0..60 {
                    append(&marker(&report), "x");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Some("outlive") => {
                Windows
                    .spawn_detached(&mut appender(&report))
                    .expect("the detached spawn");
            }
            Some("console-of") => {
                use windows_sys::Win32::Foundation::ERROR_INVALID_HANDLE;
                use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole};
                let pid: u32 = words.next().and_then(|p| p.parse().ok()).expect("a pid");
                // SAFETY: attaching to another process's console, or failing to, touches only this
                // helper, which has no console of its own and exits right after.
                let attached = unsafe { AttachConsole(pid) } != 0;
                let error = io::Error::last_os_error().raw_os_error();
                if attached {
                    // SAFETY: detaches what the call above attached.
                    unsafe { FreeConsole() };
                    append(&report, "attached");
                } else if error == Some(ERROR_INVALID_HANDLE as i32) {
                    append(&report, "no-console");
                } else {
                    append(&report, &format!("error {error:?}"));
                }
            }
            Some("ctrl") => {
                use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;
                let flag = Windows.install_shutdown().expect("a console of its own");
                for (name, event) in [("ctrl-c", CTRL_C_EVENT), ("ctrl-break", CTRL_BREAK_EVENT)] {
                    SHUTDOWN.store(false, Ordering::SeqCst);
                    // SAFETY: group 0 is every process sharing this console, and this helper is
                    // alone in a console created for it.
                    let sent = unsafe { GenerateConsoleCtrlEvent(event, 0) } != 0;
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !flag.is_set() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    append(
                        &report,
                        &format!("{name} sent={sent} caught={}", flag.is_set()),
                    );
                }
            }
            Some("close") => {
                use windows_sys::Win32::System::Console::GetConsoleWindow;
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    GetClassNameW, PostMessageW, SW_HIDE, ShowWindow, WM_CLOSE,
                };
                let flag = Windows.install_shutdown().expect("a console of its own");
                // SAFETY: no arguments; null when the console has no window.
                let window = unsafe { GetConsoleWindow() };
                assert!(!window.is_null(), "a console created with a window");
                let mut class = [0u16; 256];
                // SAFETY: the same window, and a buffer of the length passed.
                let len = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
                let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
                if class != CONSOLE_WINDOW_CLASS {
                    append(&report, &format!("window-class {class}"));
                    return;
                }
                // SAFETY: the window of this process's own console; hiding it first keeps a run
                // on an interactive desktop to a flash.
                unsafe { ShowWindow(window, SW_HIDE) };
                // SAFETY: the same window. `WM_CLOSE` is what the window's close button sends.
                let posted = unsafe { PostMessageW(window, WM_CLOSE, 0, 0) } != 0;
                let deadline = Instant::now() + Duration::from_secs(4);
                while !flag.is_set() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                append(
                    &report,
                    &format!("close posted={posted} caught={}", flag.is_set()),
                );
                // What `serve` does next: flush while the handler holds, then end the process.
                std::thread::sleep(Duration::from_secs(1));
                append(&report, "flushed");
                std::process::exit(0);
            }
            Some("job") => {
                let breakaway_ok = words.next() == Some("breakaway-ok");
                let job = kill_on_close_job(breakaway_ok);
                let child = Windows
                    .spawn_detached(&mut appender(&report))
                    .expect("the detached spawn, with or without breakaway");
                append(&report, &format!("in-job={}", in_job(&child, job)));
                // Exiting closes the last handle to the job, which ends whatever is still in it.
            }
            other => panic!("unknown helper role {other:?}"),
        }
    }

    /// A kill-on-close job, allowing breakaway when asked, with this process assigned. The handle
    /// is never closed: the job closes when this helper exits, which is what the test is about.
    fn kill_on_close_job(breakaway_ok: bool) -> HANDLE {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: an anonymous job with default security.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        assert!(!job.is_null(), "{}", io::Error::last_os_error());
        // SAFETY: an all-zero limit structure is the "no limit" state; the flags are set below.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = match breakaway_ok {
            true => JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK,
            false => JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: the structure and its size match the information class.
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        assert_ne!(set, 0, "{}", io::Error::last_os_error());
        // SAFETY: this process's pseudo handle, into a job this process owns.
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        assert_ne!(assigned, 0, "{}", io::Error::last_os_error());
        job
    }

    fn in_job(child: &DetachedChild, job: HANDLE) -> bool {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;
        let process = match &child.child {
            crate::platform::Spawned::Handle(process) => process.as_raw_handle(),
            crate::platform::Spawned::Std(child) => child.as_raw_handle(),
        };
        let mut result: BOOL = 0;
        // SAFETY: a live process handle this process holds, a job handle it owns, and an out
        // parameter it owns.
        let ok = unsafe { IsProcessInJob(process, job, &mut result) };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        result != 0
    }

    fn scratch_report(name: &str) -> (PathBuf, PathBuf) {
        let dir = scratch_dir(name);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let report = dir.join("report.txt");
        (dir, report)
    }

    fn run_helper(role: &str, report: &Path, flags: u32) -> std::process::ExitStatus {
        helper_command(role, report)
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run the helper")
    }

    fn marker_len(report: &Path) -> u64 {
        std::fs::metadata(marker(report)).map_or(0, |m| m.len())
    }

    /// Waits until the marker exists and then grows once more: the grandchild is running.
    fn assert_keeps_running(report: &Path, what: &str) {
        wait_for("the detached grandchild to start", || {
            marker_len(report) > 0
        });
        let seen = marker_len(report);
        wait_for(what, || marker_len(report) > seen);
    }

    fn wait_until_the_marker_settles(report: &Path) {
        let mut last = marker_len(report);
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let now = marker_len(report);
            if now == last {
                return;
            }
            last = now;
        }
    }

    /// The parent is a second copy of this test binary that spawns and exits; the grandchild's
    /// marker has to keep growing after that.
    #[test]
    fn a_spawned_child_outlives_its_parent() {
        let (dir, report) = scratch_report("win-outlive");
        let status = run_helper("outlive", &report, 0);
        assert!(
            status.success(),
            "the intermediate parent spawned: {status}"
        );
        assert_keeps_running(
            &report,
            "the child to go on running after its parent exited",
        );
        wait_until_the_marker_settles(&report);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `DETACHED_PROCESS` gives the child no console. Asked from a helper with no console of its
    /// own, because `AttachConsole` refuses a caller that already has one.
    #[test]
    fn a_detached_child_has_no_console() {
        let (dir, report) = scratch_report("win-console");
        let mut child = Windows
            .spawn_detached(&mut appender(&report))
            .expect("the detached spawn");
        wait_for("the detached child to start", || marker_len(&report) > 0);
        let status = run_helper(
            &format!("console-of {}", child.pid()),
            &report,
            DETACHED_PROCESS,
        );
        assert!(status.success(), "the console probe ran: {status}");
        assert_eq!(
            std::fs::read_to_string(&report)
                .expect("the probe's report")
                .trim(),
            "no-console",
            "a detached daemon has no console to show or to lose"
        );
        child.wait().ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_job_that_allows_breakaway_loses_the_child_and_one_that_does_not_keeps_it() {
        let (dir, report) = scratch_report("win-job-ok");
        let status = run_helper("job breakaway-ok", &report, 0);
        assert!(status.success(), "{status}");
        assert_eq!(
            std::fs::read_to_string(&report).expect("report").trim(),
            "in-job=false",
            "the breakaway attempt took the child out of the job"
        );
        assert_keeps_running(&report, "the child to survive the close of the job it left");
        wait_until_the_marker_settles(&report);
        std::fs::remove_dir_all(&dir).ok();

        let (dir, report) = scratch_report("win-job-kill");
        let status = run_helper("job no-breakaway", &report, 0);
        assert!(status.success(), "{status}");
        assert_eq!(
            std::fs::read_to_string(&report).expect("report").trim(),
            "in-job=true",
            "the refused breakaway was retried without the flag, and the child started"
        );
        // The helper has exited and its job closed, so the child is gone and the marker stops.
        std::thread::sleep(Duration::from_millis(300));
        let settled = marker_len(&report);
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(
            marker_len(&report),
            settled,
            "a child left in a kill-on-close job ends with it: the case the breakaway exists for"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Whether the read end of a pipe reaches end of file within `wait` after this process made
    /// the write end inheritable, ran `spawn` and closed its copy: only if the child did not
    /// inherit one.
    fn eof_after_spawning(spawn: impl FnOnce(), wait: Duration) -> bool {
        use std::io::Read as _;
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        let (mut reader, writer) = std::io::pipe().expect("a pipe");
        // SAFETY: a handle this process owns; only its inheritance flag changes.
        let ok = unsafe {
            SetHandleInformation(
                writer.as_raw_handle(),
                HANDLE_FLAG_INHERIT,
                HANDLE_FLAG_INHERIT,
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        spawn();
        drop(writer);
        let (sent, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            let _ = sent.send(reader.read_to_end(&mut rest).is_ok());
        });
        got.recv_timeout(wait).unwrap_or(false)
    }

    /// An inheritable pipe of the parent does not reach the daemon, so its reader sees end of file
    /// while the child runs. The control spawns through `Command`, and the reader waits for the
    /// child's exit: the leak the raw `CreateProcessW` stops.
    #[test]
    fn the_detached_child_inherits_no_handle_but_its_null_stdio() {
        let (dir, report) = scratch_report("win-handles");
        let mut child = None;
        let eof = eof_after_spawning(
            || {
                child = Some(
                    Windows
                        .spawn_detached(&mut appender(&report))
                        .expect("the detached spawn"),
                );
            },
            Duration::from_secs(2),
        );
        assert!(
            eof,
            "the reader saw end of file while the detached child ran"
        );
        assert_keeps_running(
            &report,
            "the child to run, while its parent's pipe was closed",
        );
        child.expect("spawned").wait().expect("the child ends");

        let (control_dir, control_report) = scratch_report("win-handles-control");
        let mut leaked = None;
        let eof = eof_after_spawning(
            || {
                leaked = Some(
                    appender(&control_report)
                        .creation_flags(DETACHED_PROCESS)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .expect("the control spawn"),
                );
            },
            Duration::from_secs(1),
        );
        assert!(
            !eof,
            "the control: a `Command` child holds the inherited pipe, so the reader waits"
        );
        leaked.expect("spawned").wait().expect("the control ends");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&control_dir).ok();
    }

    #[test]
    fn arguments_are_quoted_by_the_c_runtime_rules() {
        let cases: [(&str, &str); 7] = [
            ("serve", "serve"),
            ("", "\"\""),
            ("a b", "\"a b\""),
            ("a\"b", "\"a\\\"b\""),
            ("C:\\dir with space\\", "\"C:\\dir with space\\\\\""),
            ("a\\\\\"b", "\"a\\\\\\\\\\\"b\""),
            ("C:\\plain\\path", "C:\\plain\\path"),
        ];
        for (arg, expected) in cases {
            let mut line = Vec::new();
            append_argument(&mut line, OsStr::new(arg));
            assert_eq!(String::from_utf16_lossy(&line), expected, "{arg:?}");
        }
    }

    #[test]
    fn an_environment_change_reaches_the_child() {
        let mut command = Command::new("cmd.exe");
        assert!(environment_block(&command).is_none());
        command.env("PEMU_TEST_ENV_BLOCK", "x y").env_remove("PATH");
        let block = String::from_utf16_lossy(&environment_block(&command).expect("a block"));
        let vars: Vec<&str> = block.trim_end_matches('\0').split('\0').collect();
        assert!(vars.contains(&"PEMU_TEST_ENV_BLOCK=x y"), "{vars:?}");
        assert!(
            !vars
                .iter()
                .any(|v| v.to_ascii_uppercase().starts_with("PATH=")),
            "removed without regard to case"
        );
        let mut sorted = vars.clone();
        sorted.sort_by_key(|v| v.split('=').next().unwrap_or("").to_uppercase());
        assert_eq!(vars, sorted, "sorted by name, case-insensitive");
    }

    #[test]
    fn a_spawn_that_cannot_start_reports_the_error() {
        let missing = scratch_dir("win-spawn-missing").join("no-such-program.exe");
        let e = Windows
            .spawn_detached(&mut Command::new(&missing))
            .expect_err("a program that does not exist");
        assert!(!e.is_unsupported(), "{e}");
    }

    /// Ctrl-C and Ctrl-Break stop, the three events that end the process stop and hold, anything
    /// else passes on.
    #[test]
    fn the_console_events_map_to_stop_hold_or_pass_on() {
        assert_eq!(
            Windows.shutdown_signals(),
            &[
                ShutdownSignal::Interrupt,
                ShutdownSignal::Break,
                ShutdownSignal::Close,
                ShutdownSignal::SystemShutdown
            ]
        );
        assert_eq!(control(CTRL_C_EVENT), Control::Stop);
        assert_eq!(control(CTRL_BREAK_EVENT), Control::Stop);
        for event in [CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT] {
            assert_eq!(control(event), Control::StopAndHold, "event {event}");
        }
        for event in [3, 4, 7, u32::MAX] {
            assert_eq!(control(event), Control::PassOn, "event {event}");
        }
    }

    /// The helper runs alone in a windowless console (`CREATE_NO_WINDOW`), so the events it raises
    /// reach nothing else.
    #[test]
    fn a_real_ctrl_c_and_ctrl_break_reach_the_shutdown_flag() {
        let (dir, report) = scratch_report("win-ctrl");
        let status = run_helper("ctrl", &report, CREATE_NO_WINDOW);
        assert!(status.success(), "{status}");
        assert_eq!(
            std::fs::read_to_string(&report).expect("report"),
            "ctrl-c sent=true caught=true\nctrl-break sent=true caught=true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The window class of a console that conhost draws itself.
    const CONSOLE_WINDOW_CLASS: &str = "ConsoleWindowClass";

    /// The helper gets its own console with a window (`CREATE_NEW_CONSOLE`; a `CREATE_NO_WINDOW`
    /// console has none to close) and posts `WM_CLOSE` to it, which sends `CTRL_CLOSE_EVENT` to
    /// the helper alone. Without the hold the system would end it before `flushed`.
    ///
    /// Skips when the new console is not a classic console window. In an interactive session
    /// whose default terminal is Windows Terminal (the Windows 11 default), the new console is
    /// handed to the terminal: conhost runs as a pseudoconsole, and the window `GetConsoleWindow`
    /// returns is a `PseudoConsoleWindow` that ignores `WM_CLOSE`. There the close event comes
    /// from the terminal closing the tab, which this test cannot drive.
    #[test]
    fn a_real_console_close_reaches_the_flag_and_leaves_time_to_flush() {
        use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;
        const TEST: &str = "a_real_console_close_reaches_the_flag_and_leaves_time_to_flush";
        let (dir, report) = scratch_report("win-close");
        let status = run_helper("close", &report, CREATE_NEW_CONSOLE);
        assert!(status.success(), "{status}");
        let text = std::fs::read_to_string(&report).expect("report");
        std::fs::remove_dir_all(&dir).ok();
        if let Some(class) = text.strip_prefix("window-class ") {
            println!(
                "SKIP {TEST}: a new console here has a `{}` window, not conhost's own \
                 `{CONSOLE_WINDOW_CLASS}` (a default terminal such as Windows Terminal hosts it \
                 as a pseudoconsole), and only conhost's window turns WM_CLOSE into \
                 CTRL_CLOSE_EVENT",
                class.trim_end()
            );
            return;
        }
        assert_eq!(text, "close posted=true caught=true\nflushed\n");
    }

    #[test]
    fn a_process_without_a_console_has_no_signal_path() {
        let (dir, report) = scratch_report("win-no-console");
        let status = run_helper("ctrl", &report, DETACHED_PROCESS);
        assert!(
            !status.success(),
            "the install refuses in a process with no console"
        );
        assert!(!report.exists(), "no event was raised");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_power_throttling_state_names_the_class() {
        let state = |control, state| THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: control,
            StateMask: state,
        };
        let speed = THREAD_POWER_THROTTLING_EXECUTION_SPEED;
        assert_eq!(class_of(state(0, 0)), QosClass::Default);
        assert_eq!(class_of(state(0, speed)), QosClass::Default);
        assert_eq!(class_of(state(speed, 0)), QosClass::UserInteractive);
        assert_eq!(class_of(state(speed, speed)), QosClass::Background);
    }

    #[test]
    fn a_machine_thread_asks_for_and_reads_back_the_interactive_class() {
        let classes = std::thread::spawn(|| {
            let qos = crate::platform::thread_qos();
            let before = qos.current().expect("read");
            let granted = qos.request_interactive().expect("request");
            let after = qos.current().expect("read");
            (before, granted, after)
        })
        .join()
        .expect("the thread");
        assert_eq!(
            classes,
            (
                QosClass::Default,
                QosClass::UserInteractive,
                QosClass::UserInteractive
            )
        );
    }

    // ---------------------------------------------------------------------------------------------
    // The measurement behind the thread class
    // ---------------------------------------------------------------------------------------------

    /// The efficiency class of every logical processor (group 0); a higher `EfficiencyClass` is a
    /// more performant core.
    fn efficiency_classes() -> Vec<u8> {
        use windows_sys::Win32::System::SystemInformation::{
            GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION,
        };
        let mut len = 0u32;
        // SAFETY: a size query with no buffer.
        unsafe {
            GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut len, std::ptr::null_mut(), 0)
        };
        let mut buffer = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: a buffer of at least `len` bytes, 8-byte aligned.
        let ok = unsafe {
            GetSystemCpuSetInformation(
                buffer.as_mut_ptr().cast(),
                len,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        let bytes = buffer.as_ptr().cast::<u8>();
        let mut classes = Vec::new();
        let mut at = 0usize;
        while at < len as usize {
            // SAFETY: each record starts with its own size, inside the buffer the call filled,
            // and every record of this call is 8-byte aligned.
            let entry = unsafe { &*bytes.add(at).cast::<SYSTEM_CPU_SET_INFORMATION>() };
            // SAFETY: `Type` 0 is `CpuSetInformation`, the only variant of the union.
            let set = unsafe { entry.Anonymous.CpuSet };
            let index = set.LogicalProcessorIndex as usize;
            if classes.len() <= index {
                classes.resize(index + 1, 0);
            }
            classes[index] = set.EfficiencyClass;
            at += entry.Size as usize;
        }
        classes
    }

    /// How a probe thread asks to be scheduled.
    #[derive(Clone, Copy, Debug)]
    enum Ask {
        Nothing,
        AboveNormal,
        Highest,
        HighQos,
        HighQosAndHighest,
        EcoQos,
    }

    fn apply(ask: Ask) {
        use windows_sys::Win32::System::Threading::{
            SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST,
        };
        let throttle = |state_mask| {
            let state = THREAD_POWER_THROTTLING_STATE {
                Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
                ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
                StateMask: state_mask,
            };
            // SAFETY: as in `request_interactive`.
            let ok = unsafe {
                SetThreadInformation(
                    GetCurrentThread(),
                    ThreadPowerThrottling,
                    (&raw const state).cast(),
                    size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
                )
            };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        };
        let priority = |p| {
            // SAFETY: the calling thread's pseudo handle.
            let ok = unsafe { SetThreadPriority(GetCurrentThread(), p) };
            assert_ne!(ok, 0, "{}", io::Error::last_os_error());
        };
        match ask {
            Ask::Nothing => {}
            Ask::AboveNormal => priority(THREAD_PRIORITY_ABOVE_NORMAL),
            Ask::Highest => priority(THREAD_PRIORITY_HIGHEST),
            Ask::HighQos => throttle(0),
            Ask::HighQosAndHighest => {
                throttle(0);
                priority(THREAD_PRIORITY_HIGHEST);
            }
            Ask::EcoQos => throttle(THREAD_POWER_THROTTLING_EXECUTION_SPEED),
        }
    }

    struct Placement {
        on_e: f64,
        p50: Duration,
        p99: Duration,
        overrun: f64,
    }

    /// One paced run: every 1 ms a fixed burst of work, then a wait for the next period. Measures
    /// the share of bursts that ended below the top efficiency class, the burst time, and the
    /// share of periods overrun.
    fn paced_run(ask: Ask, classes: &[u8], length: Duration) -> Placement {
        use windows_sys::Win32::System::Threading::GetCurrentProcessorNumber;
        let top = classes.iter().copied().max().unwrap_or(0);
        let classes = classes.to_vec();
        std::thread::spawn(move || {
            apply(ask);
            let period = Duration::from_millis(1);
            let start = Instant::now();
            let mut next = start + period;
            let (mut on_e, mut overrun, mut bursts) = (0usize, 0usize, Vec::new());
            let mut x = 0x2545_f491_4f6c_dd1du64;
            while start.elapsed() < length {
                let began = Instant::now();
                for _ in 0..40_000 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    x = std::hint::black_box(x);
                }
                let took = began.elapsed();
                // SAFETY: no arguments; reads the processor the thread is running on.
                let cpu = unsafe { GetCurrentProcessorNumber() } as usize;
                if classes.get(cpu).copied().unwrap_or(top) < top {
                    on_e += 1;
                }
                bursts.push(took);
                if Instant::now() > next {
                    overrun += 1;
                    next = Instant::now() + period;
                } else {
                    crate::pacing::sleep_until(next);
                    next += period;
                }
            }
            bursts.sort_unstable();
            let n = bursts.len();
            Placement {
                on_e: on_e as f64 / n as f64,
                p50: bursts[n / 2],
                p99: bursts[(n * 99 / 100).min(n - 1)],
                overrun: overrun as f64 / n as f64,
            }
        })
        .join()
        .expect("the probe thread")
    }

    /// Which request keeps a paced thread off the efficiency cores of a hybrid host. Prints a
    /// table; asserts nothing. Run with `cargo test -p pemu-host --release --lib -- --ignored
    /// --nocapture thread_class_placement` on an otherwise idle host.
    #[test]
    #[ignore = "a measurement, about three minutes on an idle hybrid host"]
    fn thread_class_placement() {
        let classes = efficiency_classes();
        let top = classes.iter().copied().max().unwrap_or(0);
        println!(
            "logical processors {}, below the top efficiency class {} ({classes:?})",
            classes.len(),
            classes.iter().filter(|c| **c < top).count()
        );
        let asks = [
            Ask::Nothing,
            Ask::AboveNormal,
            Ask::Highest,
            Ask::HighQos,
            Ask::HighQosAndHighest,
            Ask::EcoQos,
        ];
        for load in [0usize, 8, classes.len()] {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let busy: Vec<_> = (0..load)
                .map(|_| {
                    let stop = stop.clone();
                    std::thread::spawn(move || {
                        let mut x = 1u64;
                        while !stop.load(Ordering::Relaxed) {
                            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                            x = std::hint::black_box(x);
                        }
                    })
                })
                .collect();
            for round in 0..3 {
                for ask in asks {
                    let p = paced_run(ask, &classes, Duration::from_secs(3));
                    println!(
                        "load {load:2} round {round} {:<18} on-E {:5.1} %  burst p50 {:>7.1} us  \
                         p99 {:>7.1} us  overrun {:5.2} %",
                        format!("{ask:?}"),
                        p.on_e * 100.0,
                        p.p50.as_secs_f64() * 1e6,
                        p.p99.as_secs_f64() * 1e6,
                        p.overrun * 100.0
                    );
                }
            }
            stop.store(true, Ordering::Relaxed);
            for thread in busy {
                thread.join().ok();
            }
        }
    }
}
