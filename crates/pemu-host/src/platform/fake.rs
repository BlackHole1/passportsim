//! The fake host: every platform trait answered from data a test sets, recording what was asked
//! and reaching no host mechanism. It lets a test declare the console interactive, pre-set a dialog
//! answer or make an owner-only check refuse, none of which a real host allows.
//!
//! The filesystem is not faked: [`FakeOwnerOnly::create_file`] creates a real file without the
//! owner-only protection and records the call. A fake proves the caller asked and handled a
//! refusal; that the protection is real is tested against `macos::MacOs`.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    Confirmation, Console, CrashReportOptOut, CrashReports, DaemonSpawn, DetachedChild, Dialog,
    DialogAvailability, Host, OwnerOnly, PlatformError, QosClass, ShutdownFlag, ShutdownSignal,
    Signals, ThreadQos,
};

/// What a [`FakeOwnerOnly`] was asked to do, in call order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerOnlyCall {
    CreateDir(PathBuf),
    CreateNewDir(PathBuf),
    CreateFile(PathBuf),
    CreateNewFile(PathBuf),
    Check(PathBuf),
}

#[derive(Debug, Default)]
pub struct FakeOwnerOnly {
    state: Mutex<FakeOwnerOnlyState>,
}

#[derive(Debug, Default)]
struct FakeOwnerOnlyState {
    calls: Vec<OwnerOnlyCall>,
    refuse: Vec<PathBuf>,
    refuse_all: bool,
}

impl FakeOwnerOnly {
    pub fn new() -> FakeOwnerOnly {
        FakeOwnerOnly::default()
    }

    /// Makes every later [`OwnerOnly::check`] of `path` refuse, as for a file whose mode or ACL
    /// was widened after creation.
    pub fn refuse(&self, path: impl Into<PathBuf>) {
        self.lock().refuse.push(path.into());
    }

    /// Makes every later call refuse, like a host with no implementation.
    pub fn refuse_everything(&self) {
        self.lock().refuse_all = true;
    }

    pub fn calls(&self) -> Vec<OwnerOnlyCall> {
        self.lock().calls.clone()
    }

    /// Whether `path` was checked, how a test asserts "re-checked on every read".
    pub fn checked(&self, path: &Path) -> bool {
        self.lock()
            .calls
            .iter()
            .any(|c| matches!(c, OwnerOnlyCall::Check(p) if p == path))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeOwnerOnlyState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, call: OwnerOnlyCall, path: &Path) -> Result<(), PlatformError> {
        let mut state = self.lock();
        state.calls.push(call);
        if state.refuse_all {
            return Err(PlatformError::Unsupported(
                "the fake host was told to refuse every owner-only call",
            ));
        }
        if state.refuse.iter().any(|p| p == path) {
            return Err(PlatformError::wider(
                path,
                "the fake host was told to refuse this path",
            ));
        }
        Ok(())
    }
}

impl OwnerOnly for FakeOwnerOnly {
    fn create_dir(&self, path: &Path) -> Result<(), PlatformError> {
        self.record(OwnerOnlyCall::CreateDir(path.to_path_buf()), path)?;
        std::fs::create_dir_all(path).map_err(|e| PlatformError::io(path, e))
    }

    fn create_new_dir(&self, path: &Path) -> Result<(), PlatformError> {
        self.record(OwnerOnlyCall::CreateNewDir(path.to_path_buf()), path)?;
        std::fs::create_dir(path).map_err(|e| PlatformError::io(path, e))
    }

    fn create_file(&self, path: &Path) -> Result<File, PlatformError> {
        self.record(OwnerOnlyCall::CreateFile(path.to_path_buf()), path)?;
        File::create(path).map_err(|e| PlatformError::io(path, e))
    }

    fn create_new_file(&self, path: &Path) -> Result<File, PlatformError> {
        self.record(OwnerOnlyCall::CreateNewFile(path.to_path_buf()), path)?;
        File::create_new(path).map_err(|e| PlatformError::io(path, e))
    }

    fn check(&self, path: &Path) -> Result<(), PlatformError> {
        self.record(OwnerOnlyCall::Check(path.to_path_buf()), path)
    }
}

/// The detached-spawn service: it records the command line and really spawns it, stdio to the null
/// device, with no detachment (a fake cannot prove a child outlives its parent; `macos::MacOs`
/// tests that).
#[derive(Debug, Default)]
pub struct FakeDaemonSpawn {
    state: Mutex<FakeSpawnState>,
}

#[derive(Debug, Default)]
struct FakeSpawnState {
    spawns: Vec<Vec<String>>,
    refuse: bool,
}

impl FakeDaemonSpawn {
    pub fn spawns(&self) -> Vec<Vec<String>> {
        self.lock().spawns.clone()
    }

    pub fn refuse(&self) {
        self.lock().refuse = true;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeSpawnState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DaemonSpawn for FakeDaemonSpawn {
    fn spawn_detached(&self, command: &mut Command) -> Result<DetachedChild, PlatformError> {
        let mut line = vec![command.get_program().to_string_lossy().into_owned()];
        line.extend(command.get_args().map(|a| a.to_string_lossy().into_owned()));
        let mut state = self.lock();
        state.spawns.push(line);
        if state.refuse {
            return Err(PlatformError::Unsupported(
                "the fake host was told to refuse every spawn",
            ));
        }
        drop(state);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(DetachedChild::new)
            .map_err(|e| PlatformError::io(PathBuf::new(), e))
    }
}

#[derive(Debug)]
pub struct FakeSignals {
    flag: ShutdownFlag,
    installed: AtomicBool,
}

impl Default for FakeSignals {
    fn default() -> FakeSignals {
        FakeSignals {
            flag: ShutdownFlag::leaked(),
            installed: AtomicBool::new(false),
        }
    }
}

impl FakeSignals {
    /// Delivers a shutdown, the way a real `SIGTERM` or a console close event would.
    pub fn deliver_shutdown(&self) {
        self.flag.set();
    }

    pub fn installed(&self) -> bool {
        self.installed.load(Ordering::SeqCst)
    }
}

impl Signals for FakeSignals {
    /// The macOS set, so a test reads a real shape without a host.
    fn shutdown_signals(&self) -> &'static [ShutdownSignal] {
        &[ShutdownSignal::Interrupt, ShutdownSignal::Terminate]
    }

    fn install_shutdown(&self) -> Result<ShutdownFlag, PlatformError> {
        self.installed.store(true, Ordering::SeqCst);
        Ok(self.flag)
    }
}

#[derive(Debug, Default)]
pub struct FakeConsole {
    interactive: AtomicBool,
}

impl FakeConsole {
    /// Declares whether a person is at a keyboard, which no real host lets a test decide.
    pub fn set_interactive(&self, interactive: bool) {
        self.interactive.store(interactive, Ordering::SeqCst);
    }
}

impl Console for FakeConsole {
    fn is_interactive(&self) -> bool {
        self.interactive.load(Ordering::SeqCst)
    }
}

/// The confirmation-dialog service. It never answers on its own: with no answer set,
/// [`Dialog::ask`] fails, so "a human confirmed" is never a test's default, as in the product.
#[derive(Debug, Default)]
pub struct FakeDialog {
    state: Mutex<FakeDialogState>,
}

#[derive(Debug, Default)]
struct FakeDialogState {
    answer: Option<Confirmation>,
    unavailable: Option<&'static str>,
    asked: Vec<(String, String)>,
}

impl FakeDialog {
    pub fn answer_with(&self, answer: Confirmation) {
        self.lock().answer = Some(answer);
    }

    /// Declares that no interactive desktop exists, so the caller must fall through to another
    /// confirmation path.
    pub fn make_unavailable(&self, why: &'static str) {
        self.lock().unavailable = Some(why);
    }

    /// Every `(title, body)` the dialog was raised with, so a test can check the prompt.
    pub fn asked(&self) -> Vec<(String, String)> {
        self.lock().asked.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeDialogState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Dialog for FakeDialog {
    fn availability(&self) -> DialogAvailability {
        match self.lock().unavailable {
            Some(why) => DialogAvailability::Unavailable(why),
            None => DialogAvailability::Available,
        }
    }

    fn ask(&self, title: &str, body: &str) -> Result<Confirmation, PlatformError> {
        let mut state = self.lock();
        state.asked.push((title.to_string(), body.to_string()));
        if let Some(why) = state.unavailable {
            return Err(PlatformError::Unsupported(why));
        }
        state.answer.ok_or(PlatformError::Unsupported(
            "the fake dialog was raised with no answer set: a confirmation is never assumed",
        ))
    }
}

#[derive(Debug, Default)]
pub struct FakeCrashReports {
    opted_out: AtomicBool,
}

impl FakeCrashReports {
    pub fn opted_out(&self) -> bool {
        self.opted_out.load(Ordering::SeqCst)
    }
}

impl CrashReports for FakeCrashReports {
    fn opt_out(&self) -> Result<CrashReportOptOut, PlatformError> {
        self.opted_out.store(true, Ordering::SeqCst);
        Ok(CrashReportOptOut::OptedOut)
    }
}

/// The thread-class service: one class for every thread, raised to `user-interactive` by a request
/// unless the test made requests refuse.
#[derive(Debug)]
pub struct FakeThreadQos {
    state: Mutex<FakeQosState>,
}

#[derive(Debug)]
struct FakeQosState {
    class: QosClass,
    requests: usize,
    refuse: bool,
}

impl Default for FakeThreadQos {
    /// Starts where a spawned macOS thread starts: `default`.
    fn default() -> FakeThreadQos {
        FakeThreadQos {
            state: Mutex::new(FakeQosState {
                class: QosClass::Default,
                requests: 0,
                refuse: false,
            }),
        }
    }
}

impl FakeThreadQos {
    pub fn requests(&self) -> usize {
        self.lock().requests
    }

    /// Makes every later request refuse and leave the class where it is.
    pub fn refuse_requests(&self) {
        self.lock().refuse = true;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeQosState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ThreadQos for FakeThreadQos {
    fn request_interactive(&self) -> Result<QosClass, PlatformError> {
        let mut state = self.lock();
        state.requests += 1;
        if state.refuse {
            return Err(PlatformError::not_implemented_yet(
                "the fake host was told to refuse thread-class requests",
            ));
        }
        state.class = QosClass::UserInteractive;
        Ok(state.class)
    }

    fn current(&self) -> Result<QosClass, PlatformError> {
        Ok(self.lock().class)
    }
}

#[derive(Debug, Default)]
pub struct FakeHost {
    pub owner_only: FakeOwnerOnly,
    pub daemon_spawn: FakeDaemonSpawn,
    pub signals: FakeSignals,
    pub console: FakeConsole,
    pub dialog: FakeDialog,
    pub crash_reports: FakeCrashReports,
    pub thread_qos: FakeThreadQos,
}

impl FakeHost {
    pub fn new() -> FakeHost {
        FakeHost::default()
    }
}

impl Host for FakeHost {
    fn owner_only(&self) -> &dyn OwnerOnly {
        &self.owner_only
    }

    fn daemon_spawn(&self) -> &dyn DaemonSpawn {
        &self.daemon_spawn
    }

    fn signals(&self) -> &dyn Signals {
        &self.signals
    }

    fn console(&self) -> &dyn Console {
        &self.console
    }

    fn dialog(&self) -> &dyn Dialog {
        &self.dialog
    }

    fn crash_reports(&self) -> &dyn CrashReports {
        &self.crash_reports
    }

    fn thread_qos(&self) -> &dyn ThreadQos {
        &self.thread_qos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_records_calls_and_refuses_what_it_was_told_to() {
        let dir = super::super::scratch_dir("fake-owner-only");
        let host = FakeHost::new();
        host.owner_only().create_dir(&dir).expect("fake create_dir");
        let file = dir.join("token");
        drop(
            host.owner_only()
                .create_file(&file)
                .expect("fake create_file"),
        );
        host.owner_only().check(&file).expect("accepted by default");
        assert!(
            host.owner_only.checked(&file),
            "the read check was recorded"
        );

        host.owner_only.refuse(&file);
        let refused = host
            .owner_only()
            .check(&file)
            .expect_err("a path the test refused");
        assert!(
            matches!(refused, PlatformError::NotOwnerOnly { .. }),
            "{refused}"
        );
        assert_eq!(
            host.owner_only.calls(),
            vec![
                OwnerOnlyCall::CreateDir(dir.clone()),
                OwnerOnlyCall::CreateFile(file.clone()),
                OwnerOnlyCall::Check(file.clone()),
                OwnerOnlyCall::Check(file.clone()),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_fake_that_refuses_everything_is_unsupported() {
        let host = FakeHost::new();
        host.owner_only.refuse_everything();
        let e = host
            .owner_only()
            .check(Path::new("anything"))
            .expect_err("refused");
        assert!(e.is_unsupported(), "{e}");
    }

    #[test]
    fn the_fake_dialog_answers_only_what_a_test_declared() {
        let host = FakeHost::new();
        assert_eq!(host.dialog().availability(), DialogAvailability::Available);
        let refused = host
            .dialog()
            .ask("Flash the device?", "8 MB will be written.")
            .expect_err("a confirmation is never assumed");
        assert!(refused.is_unsupported(), "{refused}");

        host.dialog.answer_with(Confirmation::Declined);
        assert_eq!(
            host.dialog()
                .ask("Flash the device?", "again")
                .expect("declined"),
            Confirmation::Declined
        );
        host.dialog.answer_with(Confirmation::Confirmed);
        assert_eq!(
            host.dialog()
                .ask("Flash the device?", "again")
                .expect("confirmed"),
            Confirmation::Confirmed
        );
        assert_eq!(host.dialog.asked().len(), 3, "every raise was recorded");
    }

    #[test]
    fn an_unavailable_dialog_refuses_instead_of_confirming() {
        let host = FakeHost::new();
        host.dialog.answer_with(Confirmation::Confirmed);
        host.dialog
            .make_unavailable("no window server in this session");
        assert!(matches!(
            host.dialog().availability(),
            DialogAvailability::Unavailable(why) if why.contains("window server")
        ));
        host.dialog()
            .ask("Flash the device?", "body")
            .expect_err("an answer set in advance does not survive an unavailable dialog");
    }

    #[test]
    fn the_fake_console_reports_what_a_test_set() {
        let host = FakeHost::new();
        assert!(
            !host.console().is_interactive(),
            "not interactive by default"
        );
        host.console.set_interactive(true);
        assert!(host.console().is_interactive());
    }

    #[test]
    fn the_fake_signals_deliver_a_shutdown() {
        let host = FakeHost::new();
        assert!(!host.signals.installed());
        let flag = host.signals().install_shutdown().expect("install");
        assert!(host.signals.installed(), "the caller asked for a handler");
        assert!(!flag.is_set());
        assert_eq!(
            host.signals().shutdown_signals(),
            &[ShutdownSignal::Interrupt, ShutdownSignal::Terminate]
        );
        host.signals.deliver_shutdown();
        assert!(flag.is_set(), "the flag the caller kept sees the shutdown");
    }

    /// A program every host has that exits at once (`/bin/sh` does not exist on Windows).
    #[cfg(unix)]
    const TRIVIAL_PROGRAM: (&str, [&str; 2]) = ("/bin/sh", ["-c", "exit 0"]);
    #[cfg(not(unix))]
    const TRIVIAL_PROGRAM: (&str, [&str; 2]) = ("cmd.exe", ["/C", "exit 0"]);

    #[test]
    fn the_fake_spawn_records_the_command_line() {
        let (program, args) = TRIVIAL_PROGRAM;
        let host = FakeHost::new();
        let mut command = Command::new(program);
        command.args(args);
        let mut child = host
            .daemon_spawn()
            .spawn_detached(&mut command)
            .expect("the fake spawns what it is given");
        child.wait().expect("reap");
        let mut expected = vec![program.to_string()];
        expected.extend(args.iter().map(|a| (*a).to_string()));
        assert_eq!(host.daemon_spawn.spawns(), vec![expected]);

        host.daemon_spawn.refuse();
        let mut again = Command::new(program);
        host.daemon_spawn()
            .spawn_detached(&mut again)
            .expect_err("a host that cannot spawn");
    }

    #[test]
    fn the_fake_crash_report_opt_out_records_the_request() {
        let host = FakeHost::new();
        assert!(!host.crash_reports.opted_out());
        assert_eq!(
            host.crash_reports().opt_out().expect("opt out"),
            CrashReportOptOut::OptedOut
        );
        assert!(host.crash_reports.opted_out());
    }

    #[test]
    fn the_fake_thread_class_reads_back_what_a_request_did() {
        let host = FakeHost::new();
        assert_eq!(
            host.thread_qos().current().expect("reads"),
            QosClass::Default
        );
        assert_eq!(
            super::super::qos_report(host.thread_qos().request_interactive()),
            "user-interactive"
        );
        assert_eq!(host.thread_qos.requests(), 1);

        let refusing = FakeHost::new();
        refusing.thread_qos.refuse_requests();
        assert_eq!(
            super::super::qos_report(refusing.thread_qos().request_interactive()),
            "not-implemented"
        );
        assert_eq!(
            super::super::qos_report(refusing.thread_qos().current()),
            "default"
        );
        assert_eq!(
            super::super::qos_report(Err(PlatformError::io(
                PathBuf::new(),
                std::io::Error::from_raw_os_error(1)
            ))),
            "unreadable"
        );
    }
}
