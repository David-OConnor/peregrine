//! Commands handed to the system shell, run in the background so the window
//! stays live while they do. Their output streams into the tab's pane as it
//! arrives, lines typed meanwhile go to their stdin, and Stop (or Ctrl+C)
//! ends them. A server, a watcher, or anything else that runs until it's
//! stopped would otherwise hold the UI thread, and the whole window, forever.

use std::{
    io::{Read, Write},
    path::Path,
    process::{ChildStdin, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use chrono::Utc;
use shell::quiet_command;

use crate::{OutItem, OutType};

/// How long to keep reading output once the shell has exited. Output it wrote
/// just before exiting may still be in the pipe; but a program it started and
/// left running (`notepad`, `code .`) holds the pipe open without writing to
/// it, so we can't wait for it to close.
const EXIT_GRACE: Duration = Duration::from_millis(300);

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
    /// Index into the tab's output where this command's output starts, so a
    /// failure can recolour its stderr.
    out_start: usize,
    /// For stdout and stderr, the output item holding a line that stream has
    /// written only part of, so the rest of it continues that row even when
    /// the other stream wrote in between.
    open_items: [Option<usize>; 2],
    /// The output's length when this command last added to it, to tell when
    /// something else has since (an echoed input line), which ends the rows
    /// in `open_items`.
    out_len: usize,
    /// Set once Stop ended it, so its exit isn't reported as a failure.
    stopped: bool,
    pid: u32,
    stdin: Option<ChildStdin>,
    rx: Receiver<Msg>,
    /// Streams not yet at their end.
    open_streams: u8,
    /// Set once the shell has exited.
    exited: Option<Option<ExitStatus>>,
    grace_over: bool,
    #[cfg(windows)]
    job: Option<win::Job>,
}

impl RunningCmd {
    /// Start `line` in the system shell (PowerShell 7 on Windows, `sh`
    /// elsewhere) in `dir`. Its output will go after the first `out_len`
    /// items of the tab's output. `ctx` is repainted whenever there's news,
    /// so the output shows without waiting for the user to move the mouse.
    pub fn start(
        line: &str,
        dir: &Path,
        out_len: usize,
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

        let (tx, rx) = mpsc::channel();
        let stdin = child.stdin.take();
        let mut open_streams = 0;
        if let Some(out) = child.stdout.take() {
            read_stream(out, Stream::Stdout, tx.clone(), ctx.clone());
            open_streams += 1;
        }
        if let Some(err) = child.stderr.take() {
            read_stream(err, Stream::Stderr, tx.clone(), ctx.clone());
            open_streams += 1;
        }

        let pid = child.id();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let status = child.wait().ok();
            if tx.send(Msg::Exited(status)).is_err() {
                return;
            }
            ctx.request_repaint();
            thread::sleep(EXIT_GRACE);
            if tx.send(Msg::GraceOver).is_ok() {
                ctx.request_repaint();
            }
        });

        Ok(Self {
            line: line.to_string(),
            clone_root: None,
            out_start: out_len,
            open_items: [None, None],
            out_len,
            stopped: false,
            pid,
            stdin,
            rx,
            open_streams,
            exited: None,
            grace_over: false,
            #[cfg(windows)]
            job,
        })
    }

    /// Add the output that's arrived since the last call to `out`, the tab's
    /// pane. Returns `Some` once the command is done and all its output is
    /// in, with whether it succeeded.
    pub fn poll(&mut self, out: &mut Vec<OutItem>) -> Option<bool> {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Out(stream, text) => self.push_out(out, stream, &text),
                Msg::Eof => self.open_streams -= 1,
                Msg::Exited(status) => self.exited = Some(status),
                Msg::GraceOver => self.grace_over = true,
            }
        }
        let status = match self.exited {
            Some(status) if self.open_streams == 0 || self.grace_over => status,
            _ => return None,
        };

        let success = status.is_some_and(|s| s.success());
        if self.stopped {
            out.push(out_item("(stopped)", OutType::StdErrOk));
        } else if !success {
            // Its stderr showed as warnings while we couldn't tell; now it's
            // failed, it shows as errors, like a quick command's.
            for item in out.iter_mut().skip(self.out_start) {
                if item.type_ == OutType::StdErrOk {
                    item.type_ = OutType::StdErr;
                }
            }
        }
        Some(success)
    }

    /// Add a chunk of output to `out`. It continues the row of a line the
    /// same stream left unfinished, so a line that arrives in pieces still
    /// shows as one.
    fn push_out(&mut self, out: &mut Vec<OutItem>, stream: Stream, mut text: &str) {
        if out.len() != self.out_len {
            self.open_items = [None, None];
        }
        let open = &mut self.open_items[stream as usize];

        if let Some(i) = *open {
            let end = text.find('\n').map_or(text.len(), |n| n + 1);
            out[i].text.push_str(&text[..end]);
            text = &text[end..];
            if out[i].text.ends_with('\n') {
                *open = None;
            }
        }
        if !text.is_empty() {
            // Whether stderr is warnings or errors depends on how the command
            // exits, so until then it's warnings (see [Self::poll]).
            let type_ = match stream {
                Stream::Stdout => OutType::StdOut,
                Stream::Stderr => OutType::StdErrOk,
            };
            out.push(out_item(text, type_));
            *open = (!text.ends_with('\n')).then_some(out.len() - 1);
        }
        self.out_len = out.len();
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
        // Messages not yet polled may say it's already exited.
        while let Ok(msg) = self.rx.try_recv() {
            if let Msg::Exited(status) = msg {
                self.exited = Some(status);
            }
        }
        self.stop();
    }
}

fn out_item(text: &str, type_: OutType) -> OutItem {
    OutItem {
        text: text.to_string(),
        type_,
        dt: Utc::now(),
    }
}

/// Read one of the command's output streams on a thread, sending its text to
/// `tx` as it arrives rather than line by line, so a prompt with no newline
/// after it still shows.
fn read_stream(
    mut src: impl Read + Send + 'static,
    stream: Stream,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let mut buf = [0; 8192];
        // Read bytes not yet decoded: the start of a UTF-8 character whose
        // remaining bytes are in the next read.
        let mut pending = Vec::new();
        loop {
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
fn take_utf8(bytes: &mut Vec<u8>) -> String {
    let cut_off = match std::str::from_utf8(bytes) {
        Err(e) if e.error_len().is_none() => bytes.len() - e.valid_up_to(),
        _ => 0,
    };
    let end = bytes.len() - cut_off;
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    bytes.drain(..end);
    text
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
