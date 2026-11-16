//! Full control on Windows: a service that runs as SYSTEM and keeps a desktop agent
//! (`agent.rs`) alive in the session that owns the physical screen.
//!
//! The daemon itself is an ordinary program, and Windows keeps an ordinary program away from
//! elevated windows, UAC prompts and the lock screen. SYSTEM is not kept away, so the agent
//! does the capturing and the input for the daemon. The service does nothing else, apart from
//! sending Ctrl+Alt+Del when the agent asks: Windows accepts that only from a service.
//!
//!   plaind service install     copy plaind to Program Files, register and start the service
//!   plaind service uninstall   stop and remove it
//!   plaind service status

#![cfg(windows)]

use anyhow::{bail, ensure, Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::null;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_SEM_TIMEOUT};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};

use crate::windesk::{self, Child, SecurityDescriptor};

const NAME: &str = "plaind";
/// Set by the agent to have the service send Ctrl+Alt+Del. Only SYSTEM may open it.
const SAS_EVENT: &str = "Global\\plaind-sas";
/// The policy that lets a service send Ctrl+Alt+Del: bit 1 = services, bit 2 = accessibility apps.
const SAS_POLICY_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const SAS_POLICY: &str = "SoftwareSASGeneration";
/// Under the service's own key: what the policy was before install, to put it back.
const PARAMS_KEY: &str = r"HKLM\SYSTEM\CurrentControlSet\Services\plaind\Parameters";
const SAS_BEFORE: &str = "SasPolicyBefore";

/// `plaind service <what>`.
pub fn cli(what: Option<&str>) -> Result<()> {
    match what {
        Some("install") => noted(install()),
        Some("uninstall") => noted(uninstall()),
        Some("status") | None => {
            println!("{}", status_line());
            Ok(())
        }
        Some(other) => bail!("unknown: plaind service {other}  (install | uninstall | status)"),
    }
}

/// Started from the tray there is no terminal to print to, so a failure is also left in a file.
fn noted(outcome: Result<()>) -> Result<()> {
    let note = crate::paths::machine_dir().join("last-error.txt");
    match &outcome {
        Ok(()) => {
            let _ = std::fs::remove_file(&note);
        }
        Err(e) => {
            let _ = std::fs::create_dir_all(crate::paths::machine_dir());
            let _ = std::fs::write(&note, format!("{e:#}\n"));
        }
    }
    outcome
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// --- State, for the command line and the tray --------------------------------

pub fn is_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

fn is_running() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|m| m.open_service(NAME, ServiceAccess::QUERY_STATUS))
        .and_then(|s| s.query_status())
        .map(|s| s.current_state == ServiceState::Running)
        .unwrap_or(false)
}

/// Whether a desktop agent is serving this session right now. Does not connect to it.
pub fn agent_reachable() -> bool {
    let (name, _) = crate::agent::pipe_name();
    let name = wide(&name);
    unsafe { WaitNamedPipeW(name.as_ptr(), 1) != 0 || GetLastError() == ERROR_SEM_TIMEOUT }
}

pub fn status_line() -> String {
    let service = match (is_installed(), is_running()) {
        (false, _) => "not installed",
        (true, false) => "installed, not running",
        (true, true) => "running",
    };
    let agent = if agent_reachable() { "reachable" } else { "not reachable" };
    format!("service: {service}\ndesktop agent for this session: {agent}")
}

// --- Install / uninstall -----------------------------------------------------

fn installed_exe() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()))
        .join("plaind")
        .join("plaind.exe")
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn need_admin() -> Result<()> {
    ensure!(
        windesk::is_elevated(),
        "this needs administrator rights: run it from an Administrator terminal, or use the tray menu"
    );
    Ok(())
}

fn stop_and_wait(service: &Service) -> Result<()> {
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    let _ = service.stop(); // already stopping is fine
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.query_status()?.current_state != ServiceState::Stopped {
        ensure!(Instant::now() < deadline, "the service did not stop");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// A file a process was just using stays locked for a moment after the process ends.
fn retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut tries = 0;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if tries >= 15 => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

fn install() -> Result<()> {
    need_admin()?;
    let current = std::env::current_exe()?;
    let target = installed_exe();
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::CHANGE_CONFIG;
    let existing = manager.open_service(NAME, access).ok();
    if let Some(service) = &existing {
        stop_and_wait(service)?;
    }

    // The service runs a copy under Program Files, which only administrators can change: SYSTEM
    // must never run a file that any program could swap. It also leaves the build folder free.
    if !same_file(&current, &target) {
        let dir = target.parent().context("no install folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        retry(|| std::fs::copy(&current, &target)).with_context(|| format!("copying to {}", target.display()))?;
    }
    lock_down_machine_dir();

    let service = match existing {
        Some(service) => service,
        None => manager
            .create_service(
                &ServiceInfo {
                    name: OsString::from(NAME),
                    display_name: OsString::from("plaind full control"),
                    service_type: ServiceType::OWN_PROCESS,
                    start_type: ServiceStartType::AutoStart,
                    error_control: ServiceErrorControl::Normal,
                    executable_path: target.clone(),
                    launch_arguments: vec![OsString::from("service-run")],
                    dependencies: vec![],
                    account_name: None, // LocalSystem
                    account_password: None,
                },
                access,
            )
            .context("registering the service")?,
    };
    service.set_description(
        "Lets the plaind daemon show and control elevated windows, UAC prompts and the lock screen from the Plain phone.",
    )?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(5) }; 3]),
    })?;
    allow_sas();
    service.start(&[] as &[&OsStr]).context("starting the service")?;

    println!("plaind full control is installed and running ({}).", target.display());
    println!("Restart plaind itself if it was built before this feature existed.");
    println!("Undo with: plaind service uninstall");
    Ok(())
}

fn uninstall() -> Result<()> {
    need_admin()?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE) {
        Ok(service) => {
            stop_and_wait(&service)?;
            restore_sas();
            service.delete().context("removing the service")?;
        }
        Err(_) => println!("the service was not installed"),
    }
    let target = installed_exe();
    if same_file(&std::env::current_exe()?, &target) {
        println!("Service removed. Delete {} yourself: a program cannot delete its own file.", target.display());
        return Ok(());
    }
    if target.exists() {
        retry(|| std::fs::remove_file(&target)).with_context(|| format!("deleting {}", target.display()))?;
    }
    if let Some(dir) = target.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    println!("plaind full control is removed.");
    Ok(())
}

/// The log folder: owned by administrators, read-only for everyone else. Without this a
/// program could prepare the folder first and steer where SYSTEM writes.
fn lock_down_machine_dir() {
    let dir = crate::paths::machine_dir();
    let _ = std::fs::create_dir_all(&dir);
    // Well-known SIDs instead of names, which are translated on non-English Windows.
    let _ = Command::new("icacls").arg(&dir).args(["/setowner", "*S-1-5-32-544", "/T", "/C", "/Q"]).output();
    let _ = Command::new("icacls")
        .arg(&dir)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "*S-1-5-32-545:(OI)(CI)RX", "/Q"])
        .output();
}

// --- Ctrl+Alt+Del ------------------------------------------------------------

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    let out = Command::new("reg").args(["query", key, "/v", value]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let hex = text.split_whitespace().find_map(|w| w.strip_prefix("0x"))?;
    u32::from_str_radix(hex, 16).ok()
}

fn reg_string(key: &str, value: &str) -> Option<String> {
    let out = Command::new("reg").args(["query", key, "/v", value]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|l| l.split("REG_SZ").nth(1)).map(|v| v.trim().to_string())
}

/// Let services send Ctrl+Alt+Del, remembering what the policy was.
fn allow_sas() {
    let before = reg_dword(SAS_POLICY_KEY, SAS_POLICY);
    if before.is_some_and(|v| v & 1 != 0) {
        return; // already allowed; nothing of ours to undo later
    }
    let note = before.map(|v| v.to_string()).unwrap_or_else(|| "absent".into());
    let _ = Command::new("reg").args(["add", PARAMS_KEY, "/v", SAS_BEFORE, "/t", "REG_SZ", "/d", &note, "/f"]).output();
    let now = (before.unwrap_or(0) | 1).to_string();
    let _ = Command::new("reg")
        .args(["add", SAS_POLICY_KEY, "/v", SAS_POLICY, "/t", "REG_DWORD", "/d", &now, "/f"])
        .output();
}

fn restore_sas() {
    let Some(before) = reg_string(PARAMS_KEY, SAS_BEFORE) else { return };
    let _ = if before == "absent" {
        Command::new("reg").args(["delete", SAS_POLICY_KEY, "/v", SAS_POLICY, "/f"]).output()
    } else {
        Command::new("reg")
            .args(["add", SAS_POLICY_KEY, "/v", SAS_POLICY, "/t", "REG_DWORD", "/d", &before, "/f"])
            .output()
    };
}

/// Ask the service to send Ctrl+Alt+Del. Works only from the desktop agent (SYSTEM); `false`
/// anywhere else, or with no service.
pub fn request_sas() -> bool {
    let name = wide(SAS_EVENT);
    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if event.is_null() {
            return false;
        }
        let set = SetEvent(event) != 0;
        CloseHandle(event);
        set
    }
}

/// Wait for the agent's requests on a thread of their own.
fn listen_for_sas() {
    let Some(sd) = SecurityDescriptor::from_sddl("D:P(A;;GA;;;SY)") else { return };
    let name = wide(SAS_EVENT);
    let event = unsafe { CreateEventW(&sd.attributes(), 0, 0, name.as_ptr()) } as usize;
    if event == 0 {
        crate::plog!("service: no Ctrl+Alt+Del event: {}", std::io::Error::last_os_error());
        return;
    }
    std::thread::spawn(move || loop {
        if unsafe { WaitForSingleObject(event as _, INFINITE) } != 0 {
            return;
        }
        crate::plog!("service: Ctrl+Alt+Del");
        send_sas();
    });
}

fn send_sas() {
    unsafe {
        let lib = LoadLibraryW(wide("sas.dll").as_ptr());
        if lib.is_null() {
            return;
        }
        if let Some(f) = GetProcAddress(lib, c"SendSAS".as_ptr().cast()) {
            let send_sas: unsafe extern "system" fn(i32) = std::mem::transmute(f);
            send_sas(0); // 0: the caller is a service, not the logged-on user
        }
    }
}

// --- The service itself ------------------------------------------------------

enum Event {
    Stop,
    SessionChanged,
}

define_windows_service!(ffi_service_main, service_main);

/// `plaind service-run`: what Windows starts. Returns when the service has stopped.
pub fn run() -> Result<()> {
    service_dispatcher::start(NAME, ffi_service_main)
        .context("service-run is started by Windows; use: plaind service install")?;
    Ok(())
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = serve() {
        crate::plog!("service: stopped: {e:#}");
    }
}

fn serve() -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    let status = service_control_handler::register(NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(Event::Stop);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::SessionChange(_) => {
            let _ = tx.send(Event::SessionChanged);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let report = |state, accept| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };
    report(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN | ServiceControlAccept::SESSION_CHANGE,
    )?;
    crate::plog!("service: running");
    listen_for_sas();
    supervise(rx);
    crate::plog!("service: stopping");
    report(ServiceState::Stopped, ServiceControlAccept::empty())?;
    Ok(())
}

struct Running {
    child: Child,
    session: u32,
    /// Who was logged on when it started: its pipe is open to that person only.
    user: Option<String>,
    since: Instant,
}

/// Keep one agent alive in the console session, for whoever is logged on there.
fn supervise(events: Receiver<Event>) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            crate::plog!("service: cannot find its own file: {e}");
            return;
        }
    };
    let command = format!("\"{}\" desktop-agent", exe.display());
    // An agent that keeps dying at once is restarted less and less eagerly.
    const WAIT: [u64; 4] = [0, 1, 2, 5];
    let mut agent: Option<Running> = None;
    let mut quick_deaths = 0usize;
    let mut not_before = Instant::now();
    loop {
        let session = windesk::console_session();
        let user = session.and_then(windesk::session_user_sid);
        if let Some(a) = &agent {
            if a.child.exited() {
                quick_deaths = if a.since.elapsed() < Duration::from_secs(10) { quick_deaths + 1 } else { 0 };
                not_before = Instant::now() + Duration::from_secs(WAIT[quick_deaths.min(3)]);
                crate::plog!("service: agent {} ended by itself", a.child.pid);
                agent = None;
            } else if Some(a.session) != session || a.user != user {
                crate::plog!("service: the console session or its user changed");
                agent = None; // dropping it ends the process
                not_before = Instant::now();
            }
        }
        if agent.is_none() && Instant::now() >= not_before {
            if let Some(session) = session {
                match windesk::spawn_system_in_session(session, &command) {
                    Ok(child) => {
                        crate::plog!("service: agent {} started in session {session}", child.pid);
                        agent = Some(Running { child, session, user, since: Instant::now() });
                    }
                    Err(e) => {
                        crate::plog!("service: could not start the agent in session {session}: {e}");
                        not_before = Instant::now() + Duration::from_secs(5);
                    }
                }
            }
        }
        match events.recv_timeout(Duration::from_secs(1)) {
            Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Event::SessionChanged) | Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Run `plaind service <what>` with administrator rights, through the Windows consent prompt.
/// For the tray, where plaind itself is not elevated.
pub fn run_elevated(what: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = wide(&exe.to_string_lossy());
    let args = wide(&format!("service {what}"));
    unsafe {
        ShellExecuteW(std::ptr::null_mut(), wide("runas").as_ptr(), exe.as_ptr(), args.as_ptr(), null(), SW_HIDE);
    }
}
