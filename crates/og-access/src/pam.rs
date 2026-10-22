//! The `pam` realm: Linux system accounts (like Proxmox's root@pam), through libpam loaded at
//! run time (no build-time dependency). Service name "og-host", falling back to "login".

use std::ffi::{c_char, c_int, c_void, CStr, CString};

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int,
    appdata: *mut c_void,
}

type StartFn = unsafe extern "C" fn(*const c_char, *const c_char, *const PamConv, *mut *mut c_void) -> c_int;
type AuthFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type EndFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_SUCCESS: c_int = 0;
const PAM_CONV_ERR: c_int = 19;
const PAM_SILENT: c_int = 0x8000;
const PAM_DISALLOW_NULL_AUTHTOK: c_int = 0x1;

extern "C" fn conv(n: c_int, msgs: *mut *const PamMessage, resp: *mut *mut PamResponse, data: *mut c_void) -> c_int {
    if n <= 0 || n > 32 {
        return PAM_CONV_ERR;
    }
    unsafe {
        let out = libc::calloc(n as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse;
        if out.is_null() {
            return PAM_CONV_ERR;
        }
        let password = &*(data as *const CString);
        for i in 0..n as isize {
            let m = *msgs.offset(i);
            let style = (*m).msg_style;
            if style == PAM_PROMPT_ECHO_OFF || style == PAM_PROMPT_ECHO_ON {
                (*out.offset(i)).resp = libc::strdup(password.as_ptr());
            }
        }
        *resp = out;
    }
    PAM_SUCCESS
}

/// Check a system user's password (pam_authenticate + pam_acct_mgmt).
pub fn authenticate(user: &str, password: &str) -> Result<(), String> {
    let lib = ["libpam.so.0", "libpam.so"].iter().find_map(|n| {
        let c = CString::new(*n).unwrap();
        let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW) };
        (!h.is_null()).then_some(h)
    });
    let Some(lib) = lib else { return Err("PAM is not available on this system".into()) };
    let sym = |name: &str| unsafe { libc::dlsym(lib, CString::new(name).unwrap().as_ptr()) };
    let (s, a, m, e) = (sym("pam_start"), sym("pam_authenticate"), sym("pam_acct_mgmt"), sym("pam_end"));
    if s.is_null() || a.is_null() || m.is_null() || e.is_null() {
        return Err("libpam is missing symbols".into());
    }
    let start: StartFn = unsafe { std::mem::transmute(s) };
    let auth: AuthFn = unsafe { std::mem::transmute(a) };
    let acct: AuthFn = unsafe { std::mem::transmute(m) };
    let end: EndFn = unsafe { std::mem::transmute(e) };
    let (Ok(cuser), Ok(cpass)) = (CString::new(user), CString::new(password)) else { return Err("invalid characters".into()) };
    let service = if std::path::Path::new("/etc/pam.d/og-host").exists() { "og-host" } else { "login" };
    let csvc = CString::new(service).unwrap();
    let pc = PamConv { conv, appdata: &cpass as *const CString as *mut c_void };
    let mut h: *mut c_void = std::ptr::null_mut();
    let r = unsafe { start(csvc.as_ptr(), cuser.as_ptr(), &pc, &mut h) };
    if r != PAM_SUCCESS {
        return Err(format!("pam_start failed ({r})"));
    }
    let mut r = unsafe { auth(h, PAM_SILENT | PAM_DISALLOW_NULL_AUTHTOK) };
    if r == PAM_SUCCESS {
        r = unsafe { acct(h, PAM_SILENT) };
    }
    unsafe { end(h, r) };
    if r == PAM_SUCCESS {
        Ok(())
    } else {
        Err("authentication failure".into())
    }
}

/// Does this system account exist?
pub fn user_exists(user: &str) -> bool {
    let Ok(c) = CString::new(user) else { return false };
    let p = unsafe { libc::getpwnam(c.as_ptr()) };
    !p.is_null() && unsafe { !CStr::from_ptr((*p).pw_name).to_bytes().is_empty() }
}
