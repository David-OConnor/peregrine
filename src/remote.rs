//! A tab's SSH session: the connection being made, then the live remote
//! shell (see `shell::ssh`). Connecting runs in the background, so an
//! unreachable host costs a status line, not a frozen window.
//!
//! Remote output goes two places. Ordinary output (`ls`, a build, a log)
//! joins the tab's scrollback like a local command's. Everything is also fed
//! to a terminal emulator ([vt100]), so when a full-screen program takes
//! over (vim, nano, less, top, htop) the pane can show its screen as the
//! program drew it, and send it keys directly; see [LiveRemote::grid_mode].

use shell::ssh::{Connecting, Phase, RemoteEvent, RemoteSession};

use crate::{
    OutType,
    running::take_utf8,
    term::Replies,
    terminal::{Output, Stream},
};

/// Remote output taken into a tab per frame; more waits for the next one.
const POLL_BYTES: usize = 256 * 1024;

/// A tab's connection: being made, or up.
pub enum Remote {
    Connecting(Connecting),
    Live(Box<LiveRemote>),
}

impl Remote {
    /// `user@host`.
    pub fn label(&self) -> String {
        match self {
            Self::Connecting(c) => c.label(),
            Self::Live(l) => l.session.label(),
        }
    }

    pub fn live(&self) -> Option<&LiveRemote> {
        match self {
            Self::Live(l) => Some(l),
            Self::Connecting(_) => None,
        }
    }

    pub fn live_mut(&mut self) -> Option<&mut LiveRemote> {
        match self {
            Self::Live(l) => Some(l),
            Self::Connecting(_) => None,
        }
    }
}

/// What [LiveRemote::pump] found.
#[derive(Default)]
pub struct Pumped {
    /// The remote shell exited, or the connection dropped.
    pub closed: bool,
    /// The shell came back to its prompt.
    pub ready: bool,
    /// Output is still waiting: pump again next frame.
    pub more: bool,
}

pub struct LiveRemote {
    pub session: RemoteSession,
    /// The remote terminal's screen, for full-screen programs.
    pub screen: vt100::Parser<Replies>,
    /// The scrollback row the remote's output continues.
    stream: Stream,
    /// The start of a UTF-8 character cut off at the end of the last output.
    pending: Vec<u8>,
    /// Set as a full-screen program takes over, for the UI to give the
    /// screen keyboard focus; cleared by the UI.
    pub grid_started: bool,
}

impl LiveRemote {
    pub fn new(session: RemoteSession) -> Self {
        let (cols, rows) = session.size();
        Self {
            session,
            screen: vt100::Parser::new_with_callbacks(rows, cols, 0, Replies::default()),
            stream: Stream::default(),
            pending: Vec::new(),
            grid_started: false,
        }
    }

    /// Whether a full-screen program has the terminal: it switched to the
    /// alternate screen (vim, nano, top), or to application cursor keys (as
    /// `less` does even when it stays on the main screen, as git's pager
    /// does). The pane then shows [Self::screen] and sends keys straight to
    /// the remote. Never at the prompt, which a crashed program can leave
    /// those modes set at.
    pub fn grid_mode(&self) -> bool {
        self.busy() && self.full_screen()
    }

    fn full_screen(&self) -> bool {
        let screen = self.screen.screen();
        screen.alternate_screen() || screen.application_cursor()
    }

    /// Whether the shell is running a command (or is a plain terminal), so
    /// typed lines are input for it rather than new commands.
    pub fn busy(&self) -> bool {
        matches!(self.session.phase(), Phase::Busy | Phase::Raw)
    }

    /// Bring in the remote's news: output into `out`, the tab's scrollback,
    /// and into the screen.
    pub fn pump(&mut self, out: &mut Output) -> Pumped {
        let mut pumped = Pumped::default();
        // The session's phase is already past everything it hands over here
        // (a program's last screen update, then the prompt), so follow it
        // event by event instead.
        let mut busy = self.busy();
        let events = self.session.poll(POLL_BYTES);
        pumped.more = !events.is_empty();
        for ev in events {
            match ev {
                RemoteEvent::Output(bytes) => self.output(&bytes, busy, out),
                RemoteEvent::Ready { .. } => {
                    busy = false;
                    // A program killed in full-screen mode leaves it set.
                    if self.full_screen() {
                        self.screen.process(b"\x1b[?1049l\x1b[?1l\x1b>");
                    }
                    // Output without a final newline ends there anyway.
                    self.stream.break_line();
                    self.pending.clear();
                    pumped.ready = true;
                }
                RemoteEvent::Closed => pumped.closed = true,
            }
        }
        pumped
    }

    fn output(&mut self, bytes: &[u8], busy: bool, out: &mut Output) {
        for part in split_after_mode_changes(bytes) {
            let was_grid = busy && self.full_screen();
            self.screen.process(part);
            if !was_grid {
                // Into the scrollback too, up to and including the sequence
                // that hands the screen to a full-screen program (which the
                // scrollback ignores).
                self.pending.extend_from_slice(part);
                let text = take_utf8(&mut self.pending);
                if !text.is_empty() {
                    out.write(&text, OutType::StdOut, &mut self.stream);
                }
                if busy && self.full_screen() {
                    self.grid_started = true;
                    self.stream.break_line();
                }
            }
        }
        let replies = self.screen.callbacks_mut().take();
        self.session.send(&replies);
    }

    /// The tab's scrollback was emptied, taking the row this was writing.
    pub fn out_cleared(&mut self) {
        self.stream.break_line();
    }

    /// Run a command line at the remote prompt.
    pub fn run(&mut self, line: &str) {
        self.session.run(line);
    }

    /// Type a line into the running program (or, without a prompt hook,
    /// the remote terminal), as Enter would.
    pub fn send_line(&mut self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\r');
        self.session.send(&bytes);
    }

    /// Match the remote terminal to the pane's size, in characters.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || self.session.size() == (cols, rows) {
            return;
        }
        self.session.resize(cols, rows);
        self.screen.screen_mut().set_size(rows, cols);
    }
}

/// Split `bytes` after each private-mode sequence (`ESC [ ? … h` or `l`),
/// which is where a full-screen program can take over the screen or hand
/// it back, so each part goes to the right place.
fn split_after_mode_changes(bytes: &[u8]) -> Vec<&[u8]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while let Some(off) = bytes[i..].windows(3).position(|w| w == b"\x1b[?") {
        let params = i + off + 3;
        let Some(len) = bytes[params..]
            .iter()
            .position(|b| !(b.is_ascii_digit() || *b == b';'))
        else {
            break;
        };
        let end = params + len;
        i = end;
        if matches!(bytes[end], b'h' | b'l') {
            parts.push(&bytes[start..=end]);
            start = end + 1;
        }
    }

    if start < bytes.len() {
        parts.push(&bytes[start..]);
    }
    parts
}
