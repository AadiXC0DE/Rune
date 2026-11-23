//! A pseudo console that runs a program the way a Windows terminal does.
//!
//! The program gets a console of its own, so it reads keys and writes the
//! screen exactly as it would in Windows Terminal. What the console renders is
//! fed into the terminal engine's grid, so a test reads the screen as text
//! rather than matching escape sequences.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rune_term::engine::Grid;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

/// Time between looks at the screen while waiting for it to change.
const POLL: Duration = Duration::from_millis(20);

/// A program running in a pseudo console, with the screen it has drawn.
pub struct ConPty {
    /// The pseudo console; closed on drop, which ends the output stream.
    console: HPCON,
    /// Where keys are written for the program to read.
    input: File,
    /// The program's process.
    process: OwnedHandle,
    /// Everything the console has rendered so far.
    output: Arc<Mutex<Vec<u8>>>,
    /// The thread draining the console's output, which must never stall: the
    /// console waits for its output to be read before it accepts more.
    reader: Option<JoinHandle<()>>,
    /// The screen the rendered output describes.
    grid: Grid,
    /// How much of `output` the grid has seen.
    fed: usize,
    /// Sequences the grid refused, kept for a failure message.
    refused: Vec<String>,
}

/// Returns a pipe's read and write ends.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    #[allow(unsafe_code)]
    // SAFETY: both out-pointers are valid for writes, no security attributes
    // are passed, and the handles returned are owned by nothing else yet.
    let pair = unsafe {
        if CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    };
    Ok(pair)
}

/// Quotes one argument the way the C runtime splits a command line.
fn quote(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0_usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes = backslashes.saturating_add(1),
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2).saturating_add(1)));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2)));
    quoted.push('"');
    quoted
}

/// Encodes `NAME=value` pairs as a sorted, doubly terminated UTF-16 block.
fn environment_block(env: &[(&str, &str)]) -> Vec<u16> {
    let mut pairs: Vec<(String, &str)> = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), *value))
        .collect();
    pairs.sort_by_key(|(name, _)| name.to_uppercase());
    let mut block = Vec::new();
    for (name, value) in pairs {
        block.extend(format!("{name}={value}").encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Returns a console size from columns and rows.
fn size(cols: u16, rows: u16) -> io::Result<COORD> {
    let coordinate = |value: u16| {
        i16::try_from(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too large"))
    };
    Ok(COORD {
        X: coordinate(cols)?,
        Y: coordinate(rows)?,
    })
}

impl ConPty {
    /// Starts `argv` in a new pseudo console of `cols` by `rows`, in `cwd`,
    /// with exactly the variables in `env`.
    pub fn spawn(
        argv: &[&str],
        env: &[(&str, &str)],
        cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> io::Result<Self> {
        let (input_read, input_write) = pipe()?;
        let (output_read, output_write) = pipe()?;
        let mut console: HPCON = 0;
        #[allow(unsafe_code)]
        // SAFETY: the two handles are open pipe ends this function owns, and
        // `console` is valid for the write. The console duplicates both, so
        // they are closed below without affecting it.
        let created = unsafe {
            CreatePseudoConsole(
                size(cols, rows)?,
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &raw mut console,
            )
        };
        if created != 0 {
            return Err(io::Error::from_raw_os_error(created));
        }
        drop(input_read);
        drop(output_write);

        let result = Self::start(argv, env, cwd, console);
        let process = match result {
            Ok(process) => process,
            Err(err) => {
                #[allow(unsafe_code)]
                // SAFETY: the console was created above and nothing else holds it.
                unsafe {
                    ClosePseudoConsole(console);
                };
                return Err(err);
            }
        };

        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        let mut stream = File::from(output_read);
        let reader = std::thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            while let Ok(read) = stream.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&chunk[..read]);
            }
        });
        Ok(Self {
            console,
            input: File::from(input_write),
            process,
            output,
            reader: Some(reader),
            grid: Grid::new(cols, rows).map_err(io::Error::other)?,
            fed: 0,
            refused: Vec::new(),
        })
    }

    /// Creates the process attached to `console`.
    fn start(
        argv: &[&str],
        env: &[(&str, &str)],
        cwd: &Path,
        console: HPCON,
    ) -> io::Result<OwnedHandle> {
        let mut bytes = 0_usize;
        #[allow(unsafe_code)]
        // SAFETY: a null list asks only for the size the list needs; the call
        // reports failure by design and writes the size.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut bytes);
        }
        // Words rather than bytes, so the list is aligned for the pointers in it.
        let mut storage = vec![0_usize; bytes.div_ceil(size_of::<usize>())];
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = storage.as_mut_ptr().cast();
        #[allow(unsafe_code)]
        // SAFETY: `list` points at `bytes` writable bytes that outlive every
        // use below, and the attribute value is the console handle itself, as
        // the pseudo console attribute requires.
        unsafe {
            if InitializeProcThreadAttributeList(list, 1, 0, &raw mut bytes) == 0 {
                return Err(io::Error::last_os_error());
            }
            if UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const c_void,
                size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                let error = io::Error::last_os_error();
                DeleteProcThreadAttributeList(list);
                return Err(error);
            }
        }

        #[allow(unsafe_code)]
        // SAFETY: every field of these plain C structures is valid when zero.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(u32::MAX);
        startup.lpAttributeList = list;
        // Without this the program takes this process's standard handles when
        // they are redirected, as they are under a test runner, and writes
        // there instead of to its console.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        let mut command: Vec<u16> = argv
            .iter()
            .map(|argument| quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
            .encode_utf16()
            .chain([0])
            .collect();
        let environment = environment_block(env);
        let directory: Vec<u16> = cwd
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        #[allow(unsafe_code)]
        // SAFETY: an all-zero PROCESS_INFORMATION is a valid out value.
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        #[allow(unsafe_code)]
        // SAFETY: the command line is a writable, terminated buffer, the
        // environment block is doubly terminated UTF-16 as the flag says, the
        // directory is terminated, and the startup structure names its own
        // size and an initialized attribute list. No handles are inherited.
        let created = unsafe {
            let created = CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                directory.as_ptr(),
                &raw const startup.StartupInfo,
                &raw mut info,
            );
            let error = io::Error::last_os_error();
            DeleteProcThreadAttributeList(list);
            if created == 0 {
                return Err(error);
            }
            drop(OwnedHandle::from_raw_handle(info.hThread));
            OwnedHandle::from_raw_handle(info.hProcess)
        };
        Ok(created)
    }

    /// Writes bytes to the program as if typed.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.input.write_all(bytes)?;
        self.input.flush()
    }

    /// Feeds whatever the console rendered since the last look into the grid.
    fn catch_up(&mut self) {
        let fresh = {
            let output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
            output.get(self.fed..).unwrap_or_default().to_vec()
        };
        self.fed = self.fed.saturating_add(fresh.len());
        // The grid stops a feed at the first sequence it refuses, without
        // saying where, so bytes go in one at a time: a refusal then costs one
        // byte rather than hiding the screen drawn after it.
        for byte in &fresh {
            if let Err(err) = self.grid.feed(std::slice::from_ref(byte)) {
                let refused = err.to_string();
                if self.refused.len() < 8 && !self.refused.contains(&refused) {
                    self.refused.push(refused);
                }
            }
        }
    }

    /// Returns the screen as text, one line per row.
    pub fn screen(&mut self) -> String {
        self.catch_up();
        self.grid.text()
    }

    /// Returns everything the console has rendered, for a failure message.
    pub fn transcript(&self) -> String {
        let output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&output).into_owned()
    }

    /// Waits until the screen text contains `needle`, at most `timeout`.
    ///
    /// Returns the screen that held it, or a message with the screen as it
    /// last was.
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) -> Result<String, String> {
        self.wait_until(needle, timeout, |screen| screen.contains(needle))
    }

    /// Waits until `accept` holds for the screen text, at most `timeout`.
    pub fn wait_until(
        &mut self,
        what: &str,
        timeout: Duration,
        mut accept: impl FnMut(&str) -> bool,
    ) -> Result<String, String> {
        let started = Instant::now();
        loop {
            let screen = self.screen();
            if accept(&screen) {
                return Ok(screen);
            }
            if started.elapsed() >= timeout {
                return Err(format!(
                    "timed out after {timeout:?} waiting for {what:?}; screen:\n{screen}\n\
                     refused sequences: {:?}",
                    self.refused
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    /// Resizes the console and the grid that mirrors it.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.catch_up();
        #[allow(unsafe_code)]
        // SAFETY: the console is open until drop.
        let resized = unsafe { ResizePseudoConsole(self.console, size(cols, rows)?) };
        if resized != 0 {
            return Err(io::Error::from_raw_os_error(resized));
        }
        self.grid.resize(cols, rows).map_err(io::Error::other)
    }

    /// Waits for the program to exit, at most `timeout`, and returns its code.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<u32> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        #[allow(unsafe_code)]
        // SAFETY: the process handle is open until drop, and `code` is valid
        // for the write.
        unsafe {
            if WaitForSingleObject(self.process.as_raw_handle(), millis) != WAIT_OBJECT_0 {
                return None;
            }
            let mut code = 0_u32;
            (GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) != 0).then_some(code)
        }
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        #[allow(unsafe_code)]
        // SAFETY: both handles are open; ending a process that already exited
        // fails harmlessly. Closing the console ends the output stream, which
        // lets the reader thread finish.
        unsafe {
            TerminateProcess(self.process.as_raw_handle(), 1);
            ClosePseudoConsole(self.console);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
