//! Windows session, token and security helpers for the service and the desktop agent.

#![cfg(windows)]

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, GENERIC_ALL, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, IsWellKnownSid, SecurityIdentification, SetTokenInformation,
    TokenElevation, TokenPrimary, TokenSessionId, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, HDESK, UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
};

/// The Windows session this process runs in.
pub fn current_session() -> u32 {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

/// The session that owns the physical screen and keyboard, when there is one.
pub fn console_session() -> Option<u32> {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    (session != u32::MAX).then_some(session)
}

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn own_token() -> Option<Token> {
    let mut h: HANDLE = null_mut();
    (unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) } != 0).then(|| Token(h))
}

/// The token of whoever is logged on to `session`. Only SYSTEM may ask.
fn session_token(session: u32) -> Option<Token> {
    let mut h: HANDLE = null_mut();
    (unsafe { WTSQueryUserToken(session, &mut h) } != 0).then(|| Token(h))
}

fn with_user_sid<T>(token: &Token, f: impl FnOnce(PSID) -> T) -> Option<T> {
    let mut len = 0u32;
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut len) };
    let mut buf = vec![0u64; (len as usize).div_ceil(8)]; // u64 keeps TOKEN_USER aligned
    if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
        return None;
    }
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    Some(f(user.User.Sid))
}

fn sid_string(sid: PSID) -> Option<String> {
    unsafe {
        let mut p: *mut u16 = null_mut();
        if ConvertSidToStringSidW(sid, &mut p) == 0 {
            return None;
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        LocalFree(p.cast());
        Some(s)
    }
}

/// Whether this process runs as the SYSTEM account.
pub fn is_system() -> bool {
    own_token()
        .and_then(|t| with_user_sid(&t, |sid| unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0))
        .unwrap_or(false)
}

/// Whether this process has administrator rights switched on.
pub fn is_elevated() -> bool {
    let Some(token) = own_token() else { return false };
    let mut info = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
    let ok = unsafe { GetTokenInformation(token.0, TokenElevation, (&mut info as *mut TOKEN_ELEVATION).cast(), size, &mut len) };
    ok != 0 && info.TokenIsElevated != 0
}

/// This process's user, as an SDDL SID string.
pub fn own_user_sid() -> Option<String> {
    with_user_sid(&own_token()?, sid_string)?
}

/// The user logged on to `session`, as an SDDL SID string. `None` when nobody is, or when this
/// process is not SYSTEM.
pub fn session_user_sid(session: u32) -> Option<String> {
    with_user_sid(&session_token(session)?, sid_string)?
}

/// A security descriptor built from SDDL text.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// A plain heap block that nothing mutates after it is built.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> Option<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        };
        (ok != 0).then(|| SecurityDescriptor(sd))
    }

    /// Valid for as long as `self` lives.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// A process started by [`spawn_system_in_session`]. Dropping it ends the process.
pub struct Child {
    process: HANDLE,
    job: HANDLE,
    pub pid: u32,
}

unsafe impl Send for Child {}

impl Child {
    pub fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.process, 1);
            // gone for real before a successor asks for the same pipe name
            WaitForSingleObject(self.process, 3000);
            CloseHandle(self.process);
            if !self.job.is_null() {
                CloseHandle(self.job);
            }
        }
    }
}

/// Start `command_line` as SYSTEM inside `session`, on its normal desktop. Only a process that
/// is itself SYSTEM (the service) can: it copies its own token and moves the copy to `session`.
pub fn spawn_system_in_session(session: u32, command_line: &str) -> std::io::Result<Child> {
    let last = std::io::Error::last_os_error;
    unsafe {
        let mut own: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut own) == 0 {
            return Err(last());
        }
        let own = Token(own);
        let mut copy: HANDLE = null_mut();
        if DuplicateTokenEx(own.0, TOKEN_ALL_ACCESS, null(), SecurityIdentification, TokenPrimary, &mut copy) == 0 {
            return Err(last());
        }
        let copy = Token(copy);
        if SetTokenInformation(copy.0, TokenSessionId, (&session as *const u32).cast(), 4) == 0 {
            return Err(last());
        }

        let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
        let mut cmd: Vec<u16> = command_line.encode_utf16().chain(Some(0)).collect();
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let made = CreateProcessAsUserW(
            copy.0,
            null(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW,
            null(), // this process's environment
            null(),
            &si,
            &mut pi,
        );
        if made == 0 {
            return Err(last());
        }
        CloseHandle(pi.hThread);

        // Tie its life to ours: if this process dies, Windows closes the job and ends it.
        let job = CreateJobObjectW(null(), null());
        if !job.is_null() {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            AssignProcessToJobObject(job, pi.hProcess);
        }
        Ok(Child { process: pi.hProcess, job, pid: pi.dwProcessId })
    }
}

// --- Following the input desktop ---------------------------------------------
//
// Windows shows one desktop at a time: the normal one ("Default"), or the secure one
// ("Winlogon") while a UAC prompt or the lock screen is up. A thread captures and injects only
// on the desktop it is attached to, and only SYSTEM may attach to the secure one.

static FOLLOW: AtomicBool = AtomicBool::new(false);

/// Make this process's capture and input threads follow the input desktop. The desktop agent
/// turns this on; nothing else does, so the daemon's own capture and input are unaffected.
pub fn follow_input_desktop() {
    FOLLOW.store(true, Ordering::Relaxed);
}

pub fn following() -> bool {
    FOLLOW.load(Ordering::Relaxed)
}

pub enum Attach {
    /// Already on the input desktop, or not following at all.
    Unchanged,
    /// Moved to the desktop of this name.
    Changed(String),
    /// The input desktop is another one and this thread could not move to it. Windows refuses a
    /// thread that owns a window; a new thread, attached before it creates anything, can.
    Stuck,
}

thread_local! {
    static ATTACHED: RefCell<Option<(HDESK, String)>> = const { RefCell::new(None) };
    static REFUSED: Cell<u32> = const { Cell::new(0) };
}

/// Attach the calling thread to the desktop that has the input now.
pub fn attach_input_desktop() -> Attach {
    attach_input_desktop_after(|| {})
}

/// Like [`attach_input_desktop`]; `leaving` runs just before a move, still on the old desktop.
pub fn attach_input_desktop_after(leaving: impl FnOnce()) -> Attach {
    if !following() {
        return Attach::Unchanged;
    }
    unsafe {
        let desk = OpenInputDesktop(0, 0, GENERIC_ALL);
        if desk.is_null() {
            return Attach::Unchanged; // mid-switch; the next try gets it
        }
        let name = desktop_name(desk);
        ATTACHED.with(|attached| {
            let mut attached = attached.borrow_mut();
            if attached.as_ref().map(|(_, n)| n.as_str()) == Some(name.as_str()) {
                CloseDesktop(desk);
                return Attach::Unchanged;
            }
            leaving();
            if SetThreadDesktop(desk) == 0 {
                let code = GetLastError();
                if REFUSED.replace(code) != code {
                    crate::plog!("desktop: this thread cannot move to {name} (error {code})");
                }
                CloseDesktop(desk);
                return Attach::Stuck;
            }
            REFUSED.set(0);
            if let Some((old, _)) = attached.replace((desk, name.clone())) {
                CloseDesktop(old);
            }
            Attach::Changed(name)
        })
    }
}

unsafe fn desktop_name(desk: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    if GetUserObjectInformationW(desk, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut needed) == 0 {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Whether the kernel object behind `handle` was created by SYSTEM or by an administrator.
pub fn owner_is_privileged(handle: HANDLE) -> bool {
    unsafe {
        let mut owner: PSID = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return false;
        }
        let ok = IsWellKnownSid(owner, WinLocalSystemSid) != 0
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0;
        LocalFree(sd);
        ok
    }
}
