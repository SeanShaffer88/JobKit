//! C ABI surface — everything declared in include/jobkit.h.
//! Conventions (house standard):
//! - `*mut c_char` results are caller-owned; free with
//!   `jk_free_string`.
//! - Failures set a thread-local last-error (code +
//!   message) and return the documented sentinel (a JK_*
//!   code, or a null handle from jk_create).
//! - Nothing unwinds across the FFI boundary (catch_unwind
//!   everywhere; release profile also sets panic = "abort").
//! - A job handle IS thread-safe: all state is serialized
//!   internally, so controls may come from any thread
//!   while steps report through the same handle on the
//!   worker thread.

// The exported functions ARE the raw-pointer C ABI declared
// in include/jobkit.h; pointer validity is the documented
// contract.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::catch_unwind;
use std::sync::Arc;

use crate::{parse_config, JkResult, Job, StepFn, ERR_INTERNAL, ERR_INVALID_ARG, JK_OK};

thread_local! {
    static LAST_ERROR: RefCell<(i32, CString)> =
        RefCell::new((0, CString::new("").unwrap()));
}

pub fn set_error(code: i32, msg: &str) {
    let clean: String = msg.chars().filter(|&c| c != '\0').collect();
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = (
            code,
            CString::new(clean).unwrap_or_else(|_| CString::new("error").unwrap()),
        );
    });
}

fn clear_error() {
    set_error(JK_OK, "");
}

fn fail(code: i32, msg: &str) -> c_int {
    set_error(code, msg);
    code as c_int
}

fn panic_code() -> c_int {
    set_error(ERR_INTERNAL, "internal panic caught at the FFI boundary");
    ERR_INTERNAL as c_int
}

fn guard<T, F: FnOnce() -> T>(f: F) -> std::thread::Result<T> {
    catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn cstr<'a>(ptr: *const c_char, what: &str) -> JkResult<&'a str> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, format!("{what} must not be null")));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| (ERR_INVALID_ARG, format!("{what} must be valid UTF-8")))
}

/// Borrow the job behind a handle pointer without taking
/// ownership. Null is an error; any other pointer is the
/// caller's contract (see the header).
fn job_ref<'a>(ptr: *mut Job) -> JkResult<&'a Job> {
    if ptr.is_null() {
        return Err((ERR_INVALID_ARG, "job handle must not be null".to_string()));
    }
    Ok(unsafe { &*ptr })
}

/// Write a caller-owned string result (jk_status).
fn out_string(out: *mut *mut c_char, text: String) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_json must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    match CString::new(text) {
        Ok(s) => {
            unsafe { *out = s.into_raw() };
            clear_error();
            JK_OK as c_int
        }
        Err(_) => fail(ERR_INTERNAL, "result string contained a NUL byte"),
    }
}

#[no_mangle]
pub extern "C" fn jk_version() -> *const c_char {
    static VERSION_C: &[u8] = b"0.1.0\0";
    VERSION_C.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn jk_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().1.as_ptr())
}

#[no_mangle]
pub extern "C" fn jk_last_error_code() -> c_int {
    LAST_ERROR.with(|e| e.borrow().0) as c_int
}

/// # Safety (caller contract): `p` is null or a string this
/// library returned and has not been freed yet. Documented
/// in the header.
#[no_mangle]
pub extern "C" fn jk_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}

fn create_impl(config_json: *const c_char, out: *mut *mut Job) -> c_int {
    if out.is_null() {
        return fail(ERR_INVALID_ARG, "out_job must not be null");
    }
    unsafe { *out = std::ptr::null_mut() };
    let inner = (|| -> JkResult<Arc<Job>> {
        let text = cstr(config_json, "config_json")?;
        let cfg = parse_config(text)?;
        Ok(Job::create(cfg))
    })();
    match inner {
        Ok(job) => {
            unsafe { *out = Arc::into_raw(job) as *mut Job };
            clear_error();
            JK_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn jk_create(config_json: *const c_char, out_job: *mut *mut Job) -> c_int {
    match guard(|| create_impl(config_json, out_job)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

fn add_step_impl(
    job_ptr: *mut Job,
    name: *const c_char,
    weight: u64,
    callback: Option<StepFn>,
    user_data: *mut c_void,
) -> c_int {
    let inner = (|| -> JkResult<()> {
        let job = job_ref(job_ptr)?;
        let name = cstr(name, "name")?;
        let callback =
            callback.ok_or((ERR_INVALID_ARG, "callback must not be null".to_string()))?;
        job.add_step(name, weight, callback, user_data)
    })();
    match inner {
        Ok(()) => {
            clear_error();
            JK_OK as c_int
        }
        Err((code, msg)) => fail(code, &msg),
    }
}

#[no_mangle]
pub extern "C" fn jk_add_step(
    job: *mut Job,
    name: *const c_char,
    weight: u64,
    callback: Option<StepFn>,
    user_data: *mut c_void,
) -> c_int {
    match guard(|| add_step_impl(job, name, weight, callback, user_data)) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Apply a control operation that returns only a status.
fn control<F: FnOnce(&Job)>(job_ptr: *mut Job, f: F) -> c_int {
    match guard(|| -> c_int {
        match job_ref(job_ptr) {
            Ok(j) => {
                f(j);
                clear_error();
                JK_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn jk_start(job: *mut Job) -> c_int {
    match guard(|| {
        let inner = job_ref(job).and_then(|j| j.start());
        match inner {
            Ok(()) => {
                clear_error();
                JK_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn jk_pause(job: *mut Job) -> c_int {
    control(job, |j| j.pause())
}

#[no_mangle]
pub extern "C" fn jk_resume(job: *mut Job) -> c_int {
    control(job, |j| j.resume())
}

#[no_mangle]
pub extern "C" fn jk_cancel(job: *mut Job) -> c_int {
    control(job, |j| j.cancel())
}

#[no_mangle]
pub extern "C" fn jk_set_total(job: *mut Job, total: u64) -> c_int {
    control(job, |j| j.set_total(total))
}

#[no_mangle]
pub extern "C" fn jk_status(job: *mut Job, out_json: *mut *mut c_char) -> c_int {
    match guard(|| {
        if out_json.is_null() {
            return fail(ERR_INVALID_ARG, "out_json must not be null");
        }
        unsafe { *out_json = std::ptr::null_mut() };
        match job_ref(job) {
            Ok(j) => out_string(out_json, j.status().to_json()),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn jk_wait(job: *mut Job, timeout_ms: u32) -> c_int {
    match guard(|| {
        let inner = job_ref(job).map(|j| j.wait(timeout_ms));
        match inner {
            Ok(true) => {
                clear_error();
                JK_OK as c_int
            }
            Ok(false) => fail(
                crate::ERR_TIMEOUT,
                "wait timed out; the job is still running",
            ),
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn jk_report(job: *mut Job, units_in_step: u64) -> c_int {
    control(job, |j| j.report(units_in_step))
}

#[no_mangle]
pub extern "C" fn jk_report_delta(job: *mut Job, delta: u64) -> c_int {
    control(job, |j| j.report_delta(delta))
}

#[no_mangle]
pub extern "C" fn jk_checkpoint(job: *mut Job) -> c_int {
    match guard(|| {
        let inner = job_ref(job).map(|j| j.checkpoint());
        match inner {
            Ok(code) => {
                if code == JK_OK {
                    clear_error();
                } else {
                    set_error(code, "cancellation requested");
                }
                code as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

#[no_mangle]
pub extern "C" fn jk_fail(job: *mut Job, message: *const c_char) -> c_int {
    match guard(|| {
        let inner = (|| -> JkResult<()> {
            let j = job_ref(job)?;
            let msg = if message.is_null() {
                "step failed"
            } else {
                cstr(message, "message")?
            };
            j.fail(msg);
            Ok(())
        })();
        match inner {
            Ok(()) => {
                clear_error();
                JK_OK as c_int
            }
            Err((code, msg)) => fail(code, &msg),
        }
    }) {
        Ok(code) => code,
        Err(_) => panic_code(),
    }
}

/// Close the handle and free it. If the job is still
/// running it is cancelled and its worker is JOINED
/// before this returns — a handle never leaks a thread.
/// Null is a no-op. Anything through the handle after
/// this is a contract violation (use-after-close).
#[no_mangle]
pub extern "C" fn jk_close(job: *mut Job) {
    if !job.is_null() {
        let _ = guard(|| unsafe {
            let arc = Arc::from_raw(job as *const Job);
            arc.shutdown();
            // `arc` drops here; the worker's own Arc kept
            // the job alive until it exited, and shutdown
            // joined it, so the job is freed exactly once.
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take_string(p: *mut c_char) -> String {
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        jk_free_string(p);
        s
    }

    fn status_json(job: *mut Job) -> crate::json::Value {
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(jk_status(job, &mut out), JK_OK as c_int);
        let text = take_string(out);
        crate::json::parse(&text).unwrap()
    }

    extern "C" fn step_ok(_ud: *mut c_void, _job: *mut Job) -> c_int {
        JK_OK as c_int
    }

    extern "C" fn step_reports_half(ud: *mut c_void, job: *mut Job) -> c_int {
        let counter = unsafe { &*(ud as *const AtomicU64) };
        counter.fetch_add(1, Ordering::SeqCst);
        jk_report(job, 1); // weight is 2: reports half its units
        jk_checkpoint(job)
    }

    extern "C" fn step_never(ud: *mut c_void, _job: *mut Job) -> c_int {
        let flag = unsafe { &*(ud as *const AtomicBool) };
        flag.store(true, Ordering::SeqCst);
        JK_OK as c_int
    }

    #[test]
    fn ffi_end_to_end_status_fields() {
        let cfg = cs(r#"{"name":"ffi-job"}"#);
        let mut job: *mut Job = std::ptr::null_mut();
        assert_eq!(jk_create(cfg.as_ptr(), &mut job), JK_OK as c_int);
        assert!(!job.is_null());

        let counter = AtomicU64::new(0);
        let ud = &counter as *const AtomicU64 as *mut c_void;
        assert_eq!(
            jk_add_step(job, cs("first").as_ptr(), 2, Some(step_reports_half), ud),
            JK_OK as c_int
        );
        assert_eq!(
            jk_add_step(
                job,
                cs("second").as_ptr(),
                2,
                Some(step_ok),
                std::ptr::null_mut()
            ),
            JK_OK as c_int
        );

        let st = status_json(job);
        assert_eq!(st.get("name").unwrap().as_str(), Some("ffi-job"));
        assert_eq!(st.get("state").unwrap().as_str(), Some("created"));
        assert_eq!(st.get("step_count").unwrap().as_i64(), Some(2));
        assert_eq!(st.get("total_units").unwrap().as_i64(), Some(4));
        assert_eq!(st.get("percent").unwrap().as_i64(), Some(0));
        assert!(st.get("eta_ms").unwrap() == &crate::json::Value::Null);
        assert!(st.get("error").unwrap() == &crate::json::Value::Null);
        assert!(st.get("step_name").is_some());
        assert!(st.get("rate_per_sec").is_some());
        assert!(st.get("elapsed_ms").is_some());

        assert_eq!(jk_start(job), JK_OK as c_int);
        assert_eq!(jk_wait(job, crate::WAIT_INFINITE), JK_OK as c_int);
        let st = status_json(job);
        assert_eq!(st.get("state").unwrap().as_str(), Some("completed"));
        assert_eq!(st.get("percent").unwrap().as_i64(), Some(100));
        assert_eq!(st.get("units_done").unwrap().as_i64(), Some(4));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        jk_close(job);
    }

    #[test]
    fn ffi_config_errors() {
        let mut job: *mut Job = std::ptr::null_mut();
        // Malformed JSON -> JK_ERR_PARSE, no handle.
        let bad = cs(r#"{"name":"#);
        assert_eq!(jk_create(bad.as_ptr(), &mut job), crate::ERR_PARSE as c_int);
        assert!(job.is_null());
        // Unknown key -> JK_ERR_PARSE.
        let unknown = cs(r#"{"name":"x","bogus":1}"#);
        assert_eq!(
            jk_create(unknown.as_ptr(), &mut job),
            crate::ERR_PARSE as c_int
        );
        // Negative total -> JK_ERR_INVALID_ARG.
        let neg = cs(r#"{"name":"x","total_units":-5}"#);
        assert_eq!(jk_create(neg.as_ptr(), &mut job), ERR_INVALID_ARG as c_int);
        // Missing name -> JK_ERR_INVALID_ARG.
        let noname = cs(r#"{}"#);
        assert_eq!(
            jk_create(noname.as_ptr(), &mut job),
            ERR_INVALID_ARG as c_int
        );
        // Null config -> JK_ERR_INVALID_ARG.
        assert_eq!(
            jk_create(std::ptr::null(), &mut job),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(jk_last_error_code(), ERR_INVALID_ARG as c_int);
    }

    #[test]
    fn ffi_null_handle_errors() {
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            jk_status(std::ptr::null_mut(), &mut out),
            ERR_INVALID_ARG as c_int
        );
        assert_eq!(jk_start(std::ptr::null_mut()), ERR_INVALID_ARG as c_int);
        assert_eq!(
            jk_checkpoint(std::ptr::null_mut()),
            ERR_INVALID_ARG as c_int
        );
        jk_close(std::ptr::null_mut()); // no-op, must not crash
    }

    #[test]
    fn ffi_add_step_after_start_is_invalid_state() {
        let cfg = cs(r#"{"name":"late-step"}"#);
        let mut job: *mut Job = std::ptr::null_mut();
        assert_eq!(jk_create(cfg.as_ptr(), &mut job), JK_OK as c_int);
        let ran = AtomicBool::new(false);
        let ud = &ran as *const AtomicBool as *mut c_void;
        assert_eq!(
            jk_add_step(job, cs("only").as_ptr(), 1, Some(step_never), ud),
            JK_OK as c_int
        );
        assert_eq!(jk_start(job), JK_OK as c_int);
        assert_eq!(
            jk_add_step(
                job,
                cs("late").as_ptr(),
                1,
                Some(step_ok),
                std::ptr::null_mut()
            ),
            crate::ERR_INVALID_STATE as c_int
        );
        assert_eq!(jk_start(job), crate::ERR_INVALID_STATE as c_int); // double start
        assert_eq!(jk_wait(job, crate::WAIT_INFINITE), JK_OK as c_int);
        jk_close(job);
    }
}
