//! Commands handed to the system shell, run in the background so the window
//! stays live while they do. Their output streams into the tab's pane as it
//! arrives, lines typed meanwhile go to their stdin, and Stop (or Ctrl+C)
//! ends them. A server, a watcher, or anything else that runs until it's
//! stopped would otherwise hold the UI thread, and the whole window, forever.

use std::{
    io::{Read, Write},
    path::Path,
    process::{ChildStdin, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use shell::quiet_command;

use crate::{
    OutType,
    terminal::{Output, Stream as OutputStream},
};

/// How long to keep reading output once the shell has exited. Output it wrote
/// just before exiting may still be in the pipe; but a program it started and
/// left running (`notepad`, `code .`) holds the pipe open without writing to
/// it, so we can't wait for it to close.
const EXIT_GRACE: Duration = Duration::from_millis(300);
/// At most ~512 KiB of normal UTF-8 output waiting for the UI per command
/// (lossy decoding can expand invalid bytes by up to three times).
const OUTPUT_QUEUE_CHUNKS: usize = 64;
const POLL_BYTES: usize = 128 * 1024;
const POLL_CHUNKS: usize = 32;

/// Which of the command's output streams a chunk came from.
#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Stdout,
    Stderr,
}

enum Msg {
    Out(Stream, String),
    /// A stream reached its end.
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
    /// First stable line ID belonging to this command, so a
    /// failure can recolour its stderr.
    out_start: u64,
    /// For stdout and stderr, the output item holding a line that stream has
    /// written only part of, so the rest of it continues that row even when
    /// the other stream wrote in between.
    streams: [OutputStream; 2],
    /// The output revision when this command last added to it, to tell when
    /// something else has since (an echoed input line), which ends the rows
    /// in `streams`.
    out_revision: u64,
    /// Set once Stop ended it, so its exit isn't reported as a failure.
    stopped: bool,
    pid: u32,
    stdin: Option<ChildStdin>,
    rx: Receiver<Msg>,
    lifecycle: Receiver<Lifecycle>,
    reader_cancel: Arc<AtomicBool>,
    ctx: egui::Context,
    /// Streams not yet at their end.
    open_streams: u8,
    /// Set once the shell has exited.
    exited: Option<Option<ExitStatus>>,
    grace_over: bool,
    last_output: Instant,
    #[cfg(windows)]
    job: Option<win::Job>,
}

impl RunningCmd {
    /// Start `line` in the system shell (PowerShell 7 on Windows, `sh`
    /// elsewhere) in `dir`. `out` supplies stable IDs for the tab's output.
    /// `ctx` is repainted whenever there's news,
    /// so the output shows without waiting for the user to move the mouse.
    pub fn start(
        line: &str,
        dir: &Path,
        out: &Output,
        ctx: &egui::Context,
    ) -> std::io::Result<Self> {
        let mut cmd = if cfg!(windows) {
            let mut c = quiet_command("pwsh");
            c.args(["-NoProfile", "-NoLogo", "-Command", line]);
            c
        } else {
            let mut c = quiet_command("sh");
            c.args(["-c", line]);
            c
        };
        cmd.current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Python fully buffers stdout when it's a pipe rather than a
            // console, so a script's prints would only show up in chunks of
            // several KB, or when it exits.
            .env("PYTHONUNBUFFERED", "1");

        // Its own process group, so Stop can signal everything it starts.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);

        let mut child = cmd.spawn()?;

        // Put the shell in a job object right away: what it starts (the
        // `python` in `python run.py`) joins the job too, so Stop can end
        // those rather than just the shell. The shell takes far longer to
        // start than this takes, so nothing escapes in between.
        #[cfg(windows)]
        let job = win::Job::new(&child);

        let (tx, rx) = mpsc::sync_channel(OUTPUT_QUEUE_CHUNKS);
        // Lifecycle messages must not wait behind a full output queue: close
        // and Stop still need to know whether the shell has already exited.
        let (lifecycle_tx, lifecycle) = mpsc::channel();
        let reader_cancel = Arc::new(AtomicBool::new(false));
        let stdin = child.stdin.take();
        let mut open_streams = 0;
        if let Some(out) = child.stdout.take() {
            read_stream(
                out,
                Stream::Stdout,
                tx.clone(),
                ctx.clone(),
                Arc::clone(&reader_cancel),
            );
            open_streams += 1;
        }
        if let Some(err) = child.stderr.take() {
            read_stream(
                err,
                Stream::Stderr,
                tx.clone(),
                ctx.clone(),
                Arc::clone(&reader_cancel),
            );
            open_streams += 1;
        }

        let pid = child.id();
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
            out_start: out.next_id(),
            streams: [OutputStream::default(), OutputStream::default()],
            out_revision: out.revision(),
            stopped: false,
            pid,
            stdin,
            rx,
            lifecycle,
            reader_cancel,
            ctx: ctx.clone(),
            open_streams,
            exited: None,
            grace_over: false,
            last_output: Instant::now(),
            #[cfg(windows)]
            job,
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
        while let Ok(msg) = self.rx.try_recv() {
            chunks += 1;
            match msg {
                Msg::Out(stream, text) => {
                    self.last_output = Instant::now();
                    bytes += text.len();
                    self.push_out(out, stream, &text);
                }
                Msg::Eof => self.open_streams -= 1,
            }
            if bytes >= POLL_BYTES
                || chunks >= POLL_CHUNKS
                || started.elapsed() >= Duration::from_millis(4)
            {
                self.ctx.request_repaint();
                return None;
            }
        }
        let status = match self.exited {
            Some(status)
                if self.open_streams == 0
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

        let success = status.is_some_and(|s| s.success());
        if self.stopped {
            out.push("(stopped)", OutType::StdErrOk);
        } else if !success {
            // Its stderr showed as warnings while we couldn't tell; now it's
            // failed, it shows as errors, like a quick command's.
            out.fail_since(self.out_start);
        }
        Some(success)
    }

    /// Add a chunk of output to `out`. It continues the row of a line the
    /// same stream left unfinished, so a line that arrives in pieces still
    /// shows as one.
    fn push_out(&mut self, out: &mut Output, stream: Stream, text: &str) {
        if out.revision() != self.out_revision {
            self.out_cleared();
        }
        let kind = match stream {
            Stream::Stdout => OutType::StdOut,
            Stream::Stderr => OutType::StdErrOk,
        };
        out.write(text, kind, &mut self.streams[stream as usize]);
        self.out_revision = out.revision();
    }

    /// The tab's output was emptied, taking what this command wrote so far
    /// with it, so the indices into it no longer point anywhere.
    pub fn out_cleared(&mut self) {
        for stream in &mut self.streams {
            stream.break_line();
        }
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

    /// Send a line typed while the command runs to its stdin. Ignored once
    /// it has closed its stdin, as a terminal would.
    pub fn send_line(&mut self, line: &str) {
        if let Some(stdin) = &mut self.stdin {
            let sent = stdin
                .write_all(format!("{line}\n").as_bytes())
                .and_then(|()| stdin.flush());
            if sent.is_err() {
                self.stdin = None;
            }
        }
    }

    /// End the command and everything it started, as Ctrl+C in a terminal
    /// would (though without giving it the chance to clean up that a Ctrl+C
    /// does). Its exit then arrives through [Self::poll] as usual.
    pub fn stop(&mut self) {
        self.reader_cancel.store(true, Ordering::Relaxed);
        if self.exited.is_some() {
            return;
        }
        self.stopped = true;

        #[cfg(windows)]
        {
            if self.job.as_ref().is_some_and(win::Job::terminate) {
                return;
            }
            // No job, so end the shell's process tree by walking it instead.
            let _ = quiet_command("taskkill")
                .args(["/T", "/F", "/PID", &self.pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(windows))]
        {
            // The whole process group: the shell was started as its leader.
            let _ = quiet_command("kill")
                .args(["-KILL", "--", &format!("-{}", self.pid)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// A command still running when its tab closes, or the app exits, goes with
/// it; nothing could show its output or stop it after that.
impl Drop for RunningCmd {
    fn drop(&mut self) {
        self.poll_lifecycle();
        self.stop();
        self.reader_cancel.store(true, Ordering::Relaxed);
        // Dropping the receiver releases any reader blocked by backpressure.
    }
}

/// Read one of the command's output streams on a thread, sending its text to
/// `tx` as it arrives rather than line by line, so a prompt with no newline
/// after it still shows.
fn read_stream(
    mut src: impl Read + Send + 'static,
    stream: Stream,
    tx: SyncSender<Msg>,
    ctx: egui::Context,
    cancelled: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut buf = [0; 8192];
        // Read bytes not yet decoded: the start of a UTF-8 character whose
        // remaining bytes are in the next read.
        let mut pending = Vec::new();
        loop {
            if cancelled.load(Ordering::Relaxed) {
                break;
            }
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => pending.extend_from_slice(&buf[..n]),
            }
            let text = take_utf8(&mut pending);
            if !text.is_empty() {
                if tx.send(Msg::Out(stream, text)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        }
        if !pending.is_empty() {
            let text = String::from_utf8_lossy(&pending).into_owned();
            let _ = tx.send(Msg::Out(stream, text));
        }
        let _ = tx.send(Msg::Eof);
        ctx.request_repaint();
    });
}

/// Decode and remove the text at the front of `bytes`, leaving behind a
/// UTF-8 character cut off at the end. Invalid bytes become U+FFFD, as with
/// [String::from_utf8_lossy].
pub(crate) fn take_utf8(bytes: &mut Vec<u8>) -> String {
    let cut_off = match std::str::from_utf8(bytes) {
        Err(e) if e.error_len().is_none() => bytes.len() - e.valid_up_to(),
        _ => 0,
    };
    let end = bytes.len() - cut_off;
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    bytes.drain(..end);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_utf8_waits_for_the_rest_of_a_character() {
        let mut pending = vec![b'a', 0xf0, 0x9f];
        assert_eq!(take_utf8(&mut pending), "a");
        pending.extend_from_slice(&[0x98, 0x80, b'b']);
        assert_eq!(take_utf8(&mut pending), "😀b");
        assert!(pending.is_empty());
    }

    #[test]
    fn a_full_queue_drains_in_bounded_passes_and_keeps_the_tail_and_stderr() {
        let script = if cfg!(windows) {
            r#"[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $s = 'x' * 90; for ($i = 0; $i -lt 12000; $i++) { [Console]::Out.WriteLine(('{0:D8}: {1}' -f $i, $s)) }; [Console]::Error.Write('warning'); [Console]::Out.WriteLine('TAIL😀'); exit 3"#
        } else {
            r#"i=0; while [ "$i" -lt 12000 ]; do printf '%08d: %090d\n' "$i" 0; i=$((i+1)); done; printf 'TAIL😀\n'; printf warning >&2; exit 3"#
        };
        let mut out = Output::default();
        let mut command = RunningCmd::start(
            script,
            &std::env::current_dir().unwrap(),
            &out,
            &egui::Context::default(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(500));
        assert!(command.poll(&mut out).is_none());
        assert!(
            out.lines.len() < 12000,
            "one frame must not drain the entire command"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let success = loop {
            if let Some(success) = command.poll(&mut out) {
                break success;
            }
            assert!(
                Instant::now() < deadline,
                "command should finish without a backpressure deadlock"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert!(!success);
        assert!(out.lines.iter().any(|l| l.text == "TAIL😀"));
        assert!(
            out.lines
                .iter()
                .any(|l| l.text == "warning" && l.kind == OutType::StdErr)
        );
    }

    #[test]
    fn stopping_a_producer_with_a_full_queue_does_not_block_drop() {
        let script = if cfg!(windows) {
            "while ($true) { [Console]::Out.Write(('x' * 8192)) }"
        } else {
            "while :; do printf '%08192d' 0; done"
        };
        let mut out = Output::default();
        let mut command = RunningCmd::start(
            script,
            &std::env::current_dir().unwrap(),
            &out,
            &egui::Context::default(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while out.lines.is_empty() {
            command.poll(&mut out);
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        command.stop();
        drop(command);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(windows)]
mod win {
    use std::{os::windows::io::AsRawHandle, process::Child, ptr::null};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
    };

    /// A Windows job object holding a command's shell and every process it
    /// starts. Closing it leaves them running: a program the command started
    /// on its own, such as an editor, should outlive it.
    pub struct Job(HANDLE);

    // SAFETY: a job handle can be used and closed from any thread.
    unsafe impl Send for Job {}

    impl Job {
        /// A new job holding `child`, or `None` if Windows refused one.
        pub fn new(child: &Child) -> Option<Self> {
            // SAFETY: plain calls on handles we own; `child`'s handle stays
            // open for the duration.
            unsafe {
                let job = CreateJobObjectW(null(), null());
                if job.is_null() {
                    return None;
                }
                if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(Self(job))
            }
        }

        /// End every process in the job. Returns whether that worked.
        pub fn terminate(&self) -> bool {
            // SAFETY: `self.0` is a job handle we own.
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a job handle we own, closed only here.
            unsafe { CloseHandle(self.0) };
        }
    }
}
