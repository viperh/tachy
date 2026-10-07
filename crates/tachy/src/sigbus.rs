//! `SIGBUS` handling (spec §16, M7-03). Unix only.
//!
//! Files are memory-mapped. If a file is truncated while it is open, reading
//! a page past its new end raises `SIGBUS`. That is a synchronous fault: a
//! "set a flag and return" handler (`signal_hook::flag`) would return to the
//! faulting load, which faults again, forever. So the handler here never
//! returns. Using only async-signal-safe calls (no allocation, no locks, no
//! `tracing`), it:
//! 1. writes precomputed escape sequences to stdout to leave the alternate
//!    screen, show the cursor, and turn off mouse capture and bracketed paste;
//! 2. restores the terminal attributes saved by [`save_termios`] before raw
//!    mode was entered (`tcsetattr`);
//! 3. writes the message prepared by [`set_message`] to stderr
//!    (`tachy: <file> was truncated while open (SIGBUS); exiting`);
//! 4. exits with `_exit(1)`.
//!
//! This is the mmap limitation of §16: tachy cannot keep showing a file that
//! shrinks under it, but it never hangs and always leaves the terminal usable.

use std::{
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicI32, AtomicPtr, Ordering},
    },
};

/// Leave the alternate screen, show the cursor, disable every mouse mode
/// crossterm enables, disable bracketed paste.
const RESTORE: &[u8] =
    b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1015l\x1b[?1006l\x1b[?2004l";

/// The terminal attributes before raw mode, and the fd they belong to.
static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();
static TTY_FD: AtomicI32 = AtomicI32::new(-1);

/// The stderr message. Points to a leaked `Box<[u8]>`: replaced messages are
/// never freed, because the handler may be reading one at any time.
static MESSAGE: AtomicPtr<Box<[u8]>> = AtomicPtr::new(ptr::null_mut());

/// Saves the terminal attributes. Call it once, **before** raw mode is
/// enabled. Uses stdin when it is a terminal, else `/dev/tty` (as crossterm
/// does when stdin is a pipe, `cat x.csv | tachy -`); that fd is kept open
/// for the handler.
pub fn save_termios() {
    // SAFETY: plain libc calls on a valid fd and a zeroed, then filled,
    // `termios`.
    unsafe {
        let fd = if libc::isatty(libc::STDIN_FILENO) == 1 {
            libc::STDIN_FILENO
        } else {
            libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return;
        }
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) == 0 && ORIGINAL_TERMIOS.set(termios).is_ok() {
            TTY_FD.store(fd, Ordering::SeqCst);
        } else if fd != libc::STDIN_FILENO {
            libc::close(fd);
        }
    }
}

/// The stderr message for the open `files`: the path when there is exactly
/// one, `the open file` otherwise (which file faulted is unknown).
pub fn message_for(files: &[&Path]) -> Vec<u8> {
    let mut msg = b"tachy: ".to_vec();
    match files {
        [one] => msg.extend_from_slice(one.as_os_str().as_bytes()),
        _ => msg.extend_from_slice(b"the open file"),
    }
    msg.extend_from_slice(b" was truncated while open (SIGBUS); exiting\n");
    msg
}

/// Prepares the message for the open `files`. Call it at startup and
/// whenever the set of open files changes (open, close). Allocates, so it
/// is never called from the handler.
pub fn set_message(files: &[&Path]) {
    let boxed = Box::new(message_for(files).into_boxed_slice());
    // The previous message is leaked on purpose (see `MESSAGE`).
    MESSAGE.store(Box::into_raw(boxed), Ordering::SeqCst);
}

/// Installs the handler. Call it before entering the TUI.
pub fn install() -> io::Result<()> {
    // SAFETY: `on_sigbus` only makes async-signal-safe calls and never
    // returns; `sa` is fully initialised.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigbus as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        if libc::sigaction(libc::SIGBUS, &sa, ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The handler. Async-signal-safe: atomics, `write`, `tcsetattr`, `_exit`.
extern "C" fn on_sigbus(_signal: libc::c_int) {
    write_all(libc::STDOUT_FILENO, RESTORE);
    if let Some(termios) = ORIGINAL_TERMIOS.get() {
        // SAFETY: a valid `termios` saved earlier; a bad fd only fails.
        unsafe {
            libc::tcsetattr(TTY_FD.load(Ordering::SeqCst), libc::TCSANOW, termios);
        }
    }
    let msg = MESSAGE.load(Ordering::SeqCst);
    if msg.is_null() {
        write_all(
            libc::STDERR_FILENO,
            b"tachy: the open file was truncated while open (SIGBUS); exiting\n",
        );
    } else {
        // SAFETY: `MESSAGE` only ever holds leaked, never freed boxes.
        write_all(libc::STDERR_FILENO, unsafe { &*msg });
    }
    // SAFETY: `_exit` is async-signal-safe and does not return.
    unsafe { libc::_exit(1) }
}

/// `write` until done, retrying on `EINTR`, giving up on any other error.
fn write_all(fd: libc::c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is a valid slice for its length.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n > 0 {
            bytes = &bytes[n as usize..];
        } else if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        } else {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Read, os::fd::FromRawFd, path::PathBuf};

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn messages() {
        let one = PathBuf::from("data/orders.csv");
        assert_eq!(
            message_for(&[&one]),
            b"tachy: data/orders.csv was truncated while open (SIGBUS); exiting\n"
        );
        let two = PathBuf::from("b.csv");
        let generic = b"tachy: the open file was truncated while open (SIGBUS); exiting\n";
        assert_eq!(message_for(&[&one, &two]), generic);
        assert_eq!(message_for(&[]), generic);
    }

    /// A real fault: a child process maps a file, truncates it and reads the
    /// mapped page. It must exit with code 1 after writing the restore
    /// sequences to stdout and the message to stderr, both captured here.
    #[test]
    fn truncated_mapping_exits_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.csv");
        std::fs::write(&path, vec![b'x'; 2 * 4096]).unwrap();
        // Everything that allocates happens before `fork`.
        set_message(&[path.as_path()]);
        let file = std::fs::File::open(&path).unwrap();
        let rw = std::fs::OpenOptions::new().write(true).open(&path).unwrap();

        // SAFETY: the child only makes async-signal-safe calls (dup2,
        // sigaction, ftruncate, a volatile read, _exit) after `fork`.
        unsafe {
            use std::os::fd::AsRawFd;
            let map = libc::mmap(
                ptr::null_mut(),
                2 * 4096,
                libc::PROT_READ,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            );
            assert_ne!(map, libc::MAP_FAILED);
            let mut out = [0; 2];
            let mut err = [0; 2];
            assert_eq!(libc::pipe(out.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(err.as_mut_ptr()), 0);

            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::dup2(out[1], libc::STDOUT_FILENO);
                libc::dup2(err[1], libc::STDERR_FILENO);
                if install().is_err() {
                    libc::_exit(3);
                }
                libc::ftruncate(rw.as_raw_fd(), 0);
                let byte = ptr::read_volatile(map.cast::<u8>().add(4096));
                // Not reached: the read faults.
                libc::_exit(if byte == b'x' { 4 } else { 5 });
            }
            libc::close(out[1]);
            libc::close(err[1]);
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            libc::munmap(map, 2 * 4096);

            let mut stdout = Vec::new();
            std::fs::File::from_raw_fd(out[0])
                .read_to_end(&mut stdout)
                .unwrap();
            let mut stderr = Vec::new();
            std::fs::File::from_raw_fd(err[0])
                .read_to_end(&mut stderr)
                .unwrap();

            assert!(libc::WIFEXITED(status), "status {status:#x}");
            assert_eq!(libc::WEXITSTATUS(status), 1);
            assert_eq!(stdout, RESTORE);
            assert_eq!(stderr, message_for(&[path.as_path()]));
        }
    }
}
