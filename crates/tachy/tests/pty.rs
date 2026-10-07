//! End-to-end tests of the real binary under a pseudo-terminal (M1-08).
//!
//! `portable-pty` (from wezterm) runs the binary on a real pty, so raw
//! mode, the alternate screen and `/dev/tty` key input work as in a
//! terminal; `vt100` parses the output back into a screen to assert on.
//! `portable-pty` was picked over `expectrl` because it gives plain
//! blocking reader / writer handles and exit codes with no regex or async
//! layer, which is all these tests need, and it is the more widely used
//! crate. The binary runs under `sh -c`, so stdin can be a pipe while the
//! keys come from the pty (`cat file | tachy -`).

#![cfg(unix)]

use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<vt100::Parser>>,
}

impl Session {
    /// Runs `script` with `sh -c` on an 80×24 pty, `$TACHY` set to the
    /// binary and the config / data directories inside `home`.
    fn spawn(script: &str, home: &Path) -> Session {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(script);
        cmd.cwd(home);
        cmd.env("TACHY", env!("CARGO_BIN_EXE_tachy"));
        cmd.env("TACHY_DATA", home);
        cmd.env("TACHY_CONFIG", home);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env_remove("NO_COLOR");
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let sink = Arc::clone(&screen);
        thread::spawn(move || {
            // Keep the master alive while reading.
            let _master = pair.master;
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().process(&buf[..n]);
            }
        });
        Session {
            child,
            writer,
            screen,
        }
    }

    fn contents(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }

    /// Waits until the screen contains every one of `needles`.
    fn wait_for(&self, needles: &[&str]) {
        let start = Instant::now();
        loop {
            let screen = self.contents();
            if needles.iter().all(|n| screen.contains(n)) {
                return;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "timed out waiting for {needles:?}; screen:\n{screen}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the screen no longer contains `needle`.
    fn wait_until_gone(&self, needle: &str) {
        let start = Instant::now();
        while self.contents().contains(needle) {
            assert!(
                start.elapsed() < TIMEOUT,
                "{needle:?} is still there; screen:\n{}",
                self.contents()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// Waits for the process to exit and returns its exit code.
    fn exit_code(&mut self) -> u32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            if start.elapsed() > TIMEOUT {
                let _ = self.child.kill();
                panic!("tachy did not exit; screen:\n{}", self.contents());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn fixture(dir: &Path, name: &str, rows: usize) -> String {
    let mut s = String::from("id,name,city\n");
    for i in 0..rows {
        s.push_str(&format!("{i},name{i},city{}\n", i % 7));
    }
    let path = dir.join(name);
    std::fs::write(&path, s).unwrap();
    path.display().to_string()
}

fn spool_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("tachy-stdin-"))
        .collect()
}

#[test]
fn stdin_is_spooled_and_keys_work() {
    let home = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let file = fixture(home.path(), "data.csv", 500);
    let script = format!(
        "cat '{file}' | \"$TACHY\" --tmp '{}' -",
        tmp.path().display()
    );
    let mut s = Session::spawn(&script, home.path());
    s.wait_for(&["stdin", "name0", "500 rows", "R 1 / 500"]);
    assert_eq!(spool_files(tmp.path()).len(), 1, "spooled while open");
    // The Detected format dialog opens over the table; Enter accepts.
    s.wait_for(&["Detected format", "delimiter        , (comma)"]);
    s.send("\r");
    s.wait_until_gone("Detected format");
    // Keys come from /dev/tty although stdin is a pipe.
    s.send("j");
    s.wait_for(&["R 2 / 500"]);
    s.send("G");
    s.wait_for(&["R 500 / 500", "500 │ 499 │"]);
    s.send("q");
    assert_eq!(s.exit_code(), 0);
    assert!(spool_files(tmp.path()).is_empty(), "spool file removed");
}

#[test]
fn several_files_open_in_tabs() {
    let home = tempfile::tempdir().unwrap();
    let a = fixture(home.path(), "a.csv", 10);
    let b = fixture(home.path(), "b.csv", 20);
    let missing = home.path().join("missing.csv").display().to_string();
    let script = format!("\"$TACHY\" -y '{a}' '{missing}' '{b}'");
    let mut s = Session::spawn(&script, home.path());
    s.wait_for(&["1 a.csv", "2 b.csv", "10 rows", "file not found"]);
    s.send("2");
    s.wait_for(&["20 rows", "R 1 / 20"]);
    s.send("\x17"); // ctrl-w closes b.csv
    s.wait_for(&["10 rows"]);
    s.send("\x17"); // closing the last tab quits
    assert_eq!(s.exit_code(), 0);
}

#[test]
fn no_file_then_ctrl_o() {
    let home = tempfile::tempdir().unwrap();
    let a = fixture(home.path(), "later.csv", 3);
    let mut s = Session::spawn("\"$TACHY\" missing.csv", home.path());
    s.wait_for(&["no file open — ctrl-o to open a file", "file not found"]);
    s.send("\x0f"); // ctrl-o
    s.wait_for(&["open ›"]);
    s.send("lat\t");
    s.wait_for(&["open › later.csv"]);
    s.send("\r");
    s.wait_for(&["1 later.csv", "3 rows", "Detected format"]);
    assert!(a.ends_with("later.csv"));
    s.send("\r");
    s.wait_until_gone("Detected format");
    s.send("q");
    assert_eq!(s.exit_code(), 0);
}

#[test]
fn the_detected_format_dialog_changes_and_reverts_the_dialect() {
    let home = tempfile::tempdir().unwrap();
    let file = fixture(home.path(), "people.csv", 50);
    let script = format!("\"$TACHY\" '{file}'");
    let mut s = Session::spawn(&script, home.path());
    s.wait_for(&["Detected format", "50 rows", "NORMAL"]);
    // `d`: tab. After the debounce the table re-parses: one column.
    s.send("d");
    s.wait_for(&["\\t (tab)*", "C 1/1"]);
    // `g` is not bound in the dialog; Esc reverts to `,`.
    s.send("\x1b");
    s.wait_until_gone("Detected format");
    s.wait_for(&["C 1/3", "city0"]);
    // The go-to dialog.
    s.send("g");
    s.wait_for(&["Go to"]);
    s.send("40\r");
    s.wait_for(&["R 40 / 50"]);
    s.send("q");
    assert_eq!(s.exit_code(), 0);
}
