//! The WebAssembly entry points and the page's imports.
//!
//! Strings cross the boundary as a pointer and a length into this module's
//! memory. A value the page returns is staged on its side: the import reports
//! its length, this side allocates that much, and `take` copies it in.
//!
//! The imports that wait, `open`, `read`, `ask`, and `run`, are wrapped by the
//! page as suspending functions, and `rune_prompt` is exported through
//! `WebAssembly.promising`. The stack suspends at those calls and resumes when
//! the page's promise settles, which is what lets blocking Rust drive an
//! asynchronous tab. `rune_cancel` and `rune_steer` touch only [`Control`], so
//! calling them while a turn is suspended does not reach the session a turn is
//! using.

#![allow(
    unsafe_code,
    reason = "a WebAssembly boundary is raw memory and foreign functions: the page \
              writes into and reads from this module's memory, and every such call is \
              unsafe by construction; each one is commented where it is made"
)]

use std::cell::RefCell;
use std::io;
use std::sync::{Arc, OnceLock};

use crate::bridge::{Bridge, Head, Ran};
use crate::host::{Config, Control, Session, encode_error};

#[link(wasm_import_module = "rune")]
unsafe extern "C" {
    fn emit(pointer: *const u8, length: usize);
    fn open(pointer: *const u8, length: usize) -> i32;
    fn read(handle: i32, pointer: *mut u8, capacity: usize) -> i32;
    safe fn close(handle: i32);
    fn ask(pointer: *const u8, length: usize) -> i32;
    fn run(pointer: *const u8, length: usize) -> i32;
    fn take(pointer: *mut u8, length: usize);
}

/// The bridge over the page's imports.
struct PageBridge;

impl PageBridge {
    /// Copies the value the page staged for the last call.
    fn staged(length: i32) -> Option<String> {
        let length = usize::try_from(length).ok()?;
        let mut buffer = vec![0_u8; length];
        // SAFETY: the buffer is `length` bytes and owned here; the page writes
        // at most that many bytes into it and keeps no reference afterwards.
        unsafe { take(buffer.as_mut_ptr(), length) };
        String::from_utf8(buffer).ok()
    }
}

impl Bridge for PageBridge {
    fn emit(&self, event: &str) {
        // SAFETY: the pointer and length describe `event`, which outlives the
        // call; the page copies the bytes before returning.
        unsafe { emit(event.as_ptr(), event.len()) };
    }

    fn open(&self, request: &serde_json::Value) -> Result<Head, String> {
        let encoded = request.to_string();
        // SAFETY: as for `emit`; the page stages its reply rather than writing
        // into this memory during the call.
        let length = unsafe { open(encoded.as_ptr(), encoded.len()) };
        let reply = Self::staged(length).ok_or_else(|| String::from("the page sent no reply"))?;
        let value: serde_json::Value =
            serde_json::from_str(&reply).map_err(|err| format!("the page's reply: {err}"))?;
        if let Some(error) = value.get("error").and_then(serde_json::Value::as_str) {
            return Err(error.to_owned());
        }
        serde_json::from_value(value).map_err(|err| format!("the page's reply: {err}"))
    }

    fn read(&self, handle: i32, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: the page writes at most `buffer.len()` bytes into `buffer`,
        // which is exclusively borrowed for the call.
        let read = unsafe { read(handle, buffer.as_mut_ptr(), buffer.len()) };
        usize::try_from(read).map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the page ended the response",
            )
        })
    }

    fn close(&self, handle: i32) {
        close(handle);
    }

    fn ask(&self, question: &serde_json::Value) -> bool {
        let encoded = question.to_string();
        // SAFETY: as for `emit`.
        unsafe { ask(encoded.as_ptr(), encoded.len()) == 1 }
    }

    fn run(&self, command: &str, cwd: &str) -> Ran {
        let encoded = serde_json::json!({ "command": command, "cwd": cwd }).to_string();
        // SAFETY: as for `open`.
        let length = unsafe { run(encoded.as_ptr(), encoded.len()) };
        Self::staged(length)
            .and_then(|reply| serde_json::from_str(&reply).ok())
            .unwrap_or_else(|| Ran {
                output: String::from("the page's shell did not answer"),
                exit_code: 127,
            })
    }
}

fn control() -> &'static Arc<Control> {
    static CONTROL: OnceLock<Arc<Control>> = OnceLock::new();
    CONTROL.get_or_init(|| Arc::new(Control::default()))
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Reads a string the page wrote into memory it allocated with `rune_alloc`.
///
/// # Safety
///
/// `pointer` and `length` must describe an allocation from [`rune_alloc`] of
/// exactly `length` bytes, which the page does not use again.
unsafe fn argument(pointer: *mut u8, length: usize) -> String {
    if pointer.is_null() || length == 0 {
        return String::new();
    }
    // SAFETY: guaranteed by the caller; `rune_alloc` made this boxed slice
    // with exactly this length, so ownership returns here and it is freed once.
    let bytes: Box<[u8]> =
        unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(pointer, length)) };
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The error for a call that needs the session while a turn holds it.
fn busy() -> rune_core::error::RuneError {
    rune_core::error::RuneError::new(
        rune_core::error::ErrorCode::Locked,
        "a turn is already running",
    )
    .with_hint("cancel it, or wait for it to finish")
}

fn report(err: &rune_core::error::RuneError) {
    PageBridge.emit(&encode_error(err).to_string());
}

/// Allocates memory for the page to write an argument into.
#[unsafe(no_mangle)]
pub extern "C" fn rune_alloc(length: usize) -> *mut u8 {
    Box::into_raw(vec![0_u8; length].into_boxed_slice()).cast::<u8>()
}

/// Starts a session, replacing any previous one. Returns zero on success.
///
/// # Safety
///
/// See [`argument`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rune_configure(pointer: *mut u8, length: usize) -> i32 {
    // SAFETY: forwarded from the caller.
    let raw = unsafe { argument(pointer, length) };
    let config: Config = match serde_json::from_str(&raw) {
        Ok(config) => config,
        Err(err) => {
            report(&rune_core::error::RuneError::invalid_field(
                "config",
                err.to_string(),
            ));
            return 1;
        }
    };
    let session = match Session::new(&config, Arc::new(PageBridge), Arc::clone(control())) {
        Ok(session) => session,
        Err(err) => {
            report(&err);
            return 1;
        }
    };
    SESSION.with(|slot| {
        let Ok(mut guard) = slot.try_borrow_mut() else {
            report(&busy());
            return 1;
        };
        *guard = Some(session);
        0
    })
}

/// Runs one prompt. Returns zero when the turn completed.
///
/// # Safety
///
/// See [`argument`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rune_prompt(pointer: *mut u8, length: usize) -> i32 {
    // SAFETY: forwarded from the caller.
    let text = unsafe { argument(pointer, length) };
    SESSION.with(|slot| {
        let Ok(mut guard) = slot.try_borrow_mut() else {
            report(&busy());
            return 1;
        };
        let Some(session) = guard.as_mut() else {
            report(&rune_core::error::RuneError::missing_field("config"));
            return 1;
        };
        match session.prompt(&text) {
            Ok(_) => 0,
            Err(err) => {
                report(&err);
                1
            }
        }
    })
}

/// Cancels the running turn, if there is one.
#[unsafe(no_mangle)]
pub extern "C" fn rune_cancel() {
    control().cancellation.cancel();
}

/// Queues a message for the running turn. Returns zero when it was queued.
///
/// # Safety
///
/// See [`argument`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rune_steer(pointer: *mut u8, length: usize) -> i32 {
    // SAFETY: forwarded from the caller.
    let text = unsafe { argument(pointer, length) };
    i32::from(control().steering.submit(text).is_err())
}

/// Forgets the conversation.
#[unsafe(no_mangle)]
pub extern "C" fn rune_reset() {
    SESSION.with(|slot| {
        if let Ok(mut guard) = slot.try_borrow_mut()
            && let Some(session) = guard.as_mut()
        {
            session.reset();
        }
    });
}
