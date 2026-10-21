//! Commands handed to the system shell, run in the background so the window
//! stays live while they do. A server, a watcher, or anything else that runs
//! until it's stopped would otherwise hold the UI thread, and the whole
//! window, forever.
//!
//! Each runs in a pseudo terminal of its own (see [crate::pty]), so it sees a
//! terminal, as it would in a terminal window: Python starts its REPL, Vim
//! draws its screen, and output comes in colour. What it writes streams into
//! the tab's pane as it arrives (see [Term]), or, while a full-screen program
//! has the terminal, the pane shows its screen. Lines typed meanwhile go to
//! it as typed input, Ctrl+C interrupts it, and Stop ends it.

use std::{
    io::{self, Read},
    path::Path,
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    OutType,
    pty::{self, Pty, Spawned},
    term::Term,
    terminal::Output,
};

/// How long to keep reading output once the shell has exited. Output it wrote
/// just before exiting may still be on its way; but a program it started and
/// left running (`code .`) can hold the terminal open without writing to it,
/// and on Windows the pseudo console holds it open until it's closed, so we
/// can't wait for its end.
const EXIT_GRACE: Duration = Duration::from_millis(300);
/// At most ~512 KiB of output waiting for the UI per command.
const OUTPUT_QUEUE_CHUNKS: usize = 64;
const POLL_BYTES: usize = 128 * 1024;
const POLL_CHUNKS: usize = 32;

enum Msg {
    Out(Vec<u8>),
    /// The output reached its end.
    Eof,
}

enum Lifecycle {
    /// The shell exited, with its status (`None` if waiting on it failed).
    Exited(Option<ExitStatus>),
    /// [EXIT_GRACE] has passed since `Exited`.
    GraceOver,
}

/// A command line running in the system shell, for one tab.
pub struct RunningCmd {
    /// The line as handed to the shell, shown by the input row while it runs.
    pub line: String,
    /// A clone URL root to save if the command succeeds (see `util::handle_cmd`).
    pub clone_root: Option<String>,
    /// Its terminal's screen, whose text is in the tab's output.
    pub term: Term,
    /// Set once Stop ended it, so its exit isn't reported as a failure.
    stopped: bool,
    pty: Pty,
    rx: Receiver<Msg>,
    lifecycle: Receiver<Lifecycle>,
    reader_cancel: Arc<AtomicBool>,
    ctx: egui::Context,
    /// Set once the output has reached its end.
    output_ended: bool,
    /// Set once the shell has exited.
    exited: Option<Option<ExitStatus>>,
    grace_over: bool,
    last_output: Instant,
}

impl RunningCmd {
    /// Start `line` in the system shell (PowerShell 7 on Windows, `sh`
    /// elsewhere) in `dir`, in a terminal of `size` (columns, rows). `out` is
    /// the tab's output, which its output joins. `ctx` is repainted whenever
    /// there's news, so the output shows without waiting for the user to
    /// move the mouse.
    pub fn start(
        line: &str,
        dir: &Path,
        size: (u16, u16),
        out: &Output,
        ctx: &egui::Context,
    ) -> io::Result<Self> {
        let (program, args) = if cfg!(windows) {
            ("pwsh", vec!["-NoProfile", "-NoLogo", "-Command", line])
        } else {
            ("sh", vec!["-c", line])
        };
        let (cols, rows) = size;
        let Spawned {
            pty,
            output,
            mut child,
        } = pty::spawn(program, &args, dir, cols, rows)?;

        let (tx, rx) = mpsc::sync_channel(OUTPUT_QUEUE_CHUNKS);
        // Lifecycle messages must not wait behind a full output queue: close
        // and Stop still need to know whether the shell has already exited.
        let (lifecycle_tx, lifecycle) = mpsc::channel();
        let reader_cancel = Arc::new(AtomicBool::new(false));
        read_output(output, tx, ctx.clone(), Arc::clone(&reader_cancel));

        let waiter_ctx = ctx.clone();
        thread::spawn(move || {
            let status = child.wait().ok();
            if lifecycle_tx.send(Lifecycle::Exited(status)).is_err() {
                return;
            }
            waiter_ctx.request_repaint();
            thread::sleep(EXIT_GRACE);
            if lifecycle_tx.send(Lifecycle::GraceOver).is_ok() {
                waiter_ctx.request_repaint();
            }
        });

        Ok(Self {
            line: line.to_string(),
            clone_root: None,
            term: Term::new(cols, rows, out),
            stopped: false,
            pty,
            rx,
            lifecycle,
            reader_cancel,
            ctx: ctx.clone(),
            output_ended: false,
            exited: None,
            grace_over: false,
            last_output: Instant::now(),
        })
    }

    /// Add the output that's arrived since the last call to `out`, the tab's
    /// pane. Returns `Some` once the command is done and all its output is
    /// in, with whether it succeeded.
    pub fn poll(&mut self, out: &mut Output) -> Option<bool> {
        self.poll_lifecycle();
        let mut bytes = 0;
        let mut chunks = 0;
        let started = Instant::now();
        let mut more = false;
        while let Ok(msg) = self.rx.try_recv() {
            chunks += 1;
            match msg {
                Msg::Out(data) => {
                    self.last_output = Instant::now();
                    bytes += data.len();
                    let replies = self.term.feed(&data, out);
                    self.send(&replies);
                }
                Msg::Eof => self.output_ended = true,
            }
            if bytes >= POLL_BYTES
                || chunks >= POLL_CHUNKS
                || started.elapsed() >= Duration::from_millis(4)
            {
                more = true;
                break;
            }
        }
        self.term.render(out);
        if more {
            self.ctx.request_repaint();
            return None;
        }

        let status = match self.exited {
            Some(status)
                if self.output_ended
                    || (self.grace_over && self.last_output.elapsed() >= EXIT_GRACE) =>
            {
                status
            }
            Some(_) => {
                self.ctx.request_repaint_after(EXIT_GRACE);
                return None;
            }
            None => return None,
        };

        self.term.finish(out);
        if self.stopped {
            out.push("(stopped)", OutType::StdErrOk);
        }
        Some(status.is_some_and(|s| s.success()))
    }

    fn poll_lifecycle(&mut self) {
        while let Ok(msg) = self.lifecycle.try_recv() {
            match msg {
                Lifecycle::Exited(status) => self.exited = Some(status),
                Lifecycle::GraceOver => {
                    self.grace_over = true;
                }
            }
        }
    }

    /// Whether a full-screen program has the terminal, so the pane shows its
    /// screen and keys go straight to it.
    pub fn full_screen(&self) -> bool {
        self.term.full_screen()
    }

    /// Send bytes to the program, as typed.
    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.pty.send(bytes);
    }

    /// Type a line into the program, as Enter would. The terminal echoes it
    /// when the program wants it echoed (not a password, say).
    pub fn send_line(&mut self, line: &str) {
        self.send(format!("{line}\r").as_bytes());
    }

    /// Ctrl+C, as in a terminal: a REPL drops its input line, most other
    /// programs exit.
    pub fn interrupt(&mut self) {
        let _ = self.pty.interrupt();
    }

    /// End the program's input, as Ctrl+D (Ctrl+Z on Windows) does on an
    /// empty line in a terminal.
    pub fn end_input(&mut self) {
        let _ = self.pty.end_input();
    }

    /// Match the terminal to the pane's size, in characters. Only while a
    /// full-screen program has it: a resize makes Windows' pseudo console
    /// repaint the screen from how it reflowed its lines, which can differ
    /// from what the pane already shows, and the emulator cuts off lines
    /// it narrows. Ordinary output is unaffected by the size it started at.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if self.full_screen() {
            self.pty.resize(cols, rows);
            self.term.resize(cols, rows);
        }
    }

    /// End the command and everything it started, without giving it the
    /// chance to clean up that an [Self::interrupt] does. Its exit then
    /// arrives through [Self::poll] as usual.
    pub fn stop(&mut self) {
        self.reader_cancel.store(true, Ordering::Relaxed);
        if self.exited.is_some() {
            return;
        }
        self.stopped = true;
        self.pty.kill();
    }
}

/// A command still running when its tab closes, or the app exits, goes with
/// it; nothing could show its output or stop it after that.
impl Drop for RunningCmd {
    fn drop(&mut self) {
        self.poll_lifecycle();
        self.stop();
        // Dropping the receiver releases a reader blocked by backpressure,
        // and the pty's closing ends the output it's waiting on.
    }
}

/// Read the terminal's output on a thread, sending it to `tx` as it arrives.
fn read_output(
    mut src: impl Read + Send + 'static,
    tx: SyncSender<Msg>,
    ctx: egui::Context,
    cancelled: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buf = [0; 8192];
        while !cancelled.load(Ordering::Relaxed) {
            match src.read(&mut buf) {
                // A Unix terminal reports its end as an error once everything
                // in it has exited.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Msg::Out(buf[..n].to_vec())).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        }
        let _ = tx.send(Msg::Eof);
        ctx.request_repaint();
    });
}

/// Decode and remove the text at the front of `bytes`, leaving behind a
/// UTF-8 character cut off at the end. Invalid bytes become U+FFFD, as with
/// [String::from_utf8_lossy].
pub fn take_utf8(bytes: &mut Vec<u8>) -> String {
    let cut_off = match std::str::from_utf8(bytes) {
        Err(e) if e.error_len().is_none() => bytes.len() - e.valid_up_to(),
        _ => 0,
    };
    let end = bytes.len() - cut_off;
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    bytes.drain(..end);
    text
}
