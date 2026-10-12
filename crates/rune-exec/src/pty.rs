//! Linux terminal allocation. All descriptors are close-on-exec.

use std::fs::File;
use std::io;
use std::process::{Command, Stdio};

use rustix::pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, unlockpt};
use rustix::termios::{Winsize, tcsetwinsize};

/// Installs a PTY slave as all three standard streams and returns the master.
pub(crate) fn configure(spec: &mut Command, rows: u16, cols: u16) -> io::Result<File> {
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = File::from(openpt(flags)?);
    grantpt(&master)?;
    unlockpt(&master)?;
    // A blocking master write can remain asleep even after the slave closes.
    // Polling nonblocking I/O lets cancellation stop both worker threads.
    rustix::io::ioctl_fionbio(&master, true)?;
    let slave = File::from(ioctl_tiocgptpeer(&master, flags)?);
    resize(&master, rows, cols)?;
    spec.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    crate::command::apply_child_setup(spec, crate::command::resource_limits(), true);
    Ok(master)
}

/// Changes the terminal size, delivering SIGWINCH to its foreground group.
pub(crate) fn resize(master: &File, rows: u16, cols: u16) -> io::Result<()> {
    if rows == 0 || cols == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal rows and cols must be nonzero",
        ));
    }
    tcsetwinsize(
        master,
        Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    Ok(())
}
