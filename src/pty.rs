//! A pseudo terminal for local commands: ConPTY on Windows, a Unix PTY
//! elsewhere. A program run in one sees a terminal, as it would in a
//! terminal window, rather than the pipes it would otherwise get: Python
//! starts its REPL, Vim draws its screen, a password prompt can turn echo
//! off, and output comes in color. What it writes comes back as the bytes a
//! terminal would be sent, escape sequences and all, for [crate::term] to
//! emulate. Its stdout and stderr are the same terminal, so they arrive
//! together, as in a terminal window.

use std::{
    fs::File,
    io::{self, Write},
    path::Path,
    process::ExitStatus,
};

/// The terminal end of a pseudo terminal with a command running in it.
pub struct Pty {
    /// What the program reads: keystrokes and typed lines.
    input: File,
    size: (u16, u16),
    sys: sys::Pty,
}

/// The command started in a [Pty], to wait for.
pub struct Child(sys::Child);

/// What [spawn] starts.
pub struct Spawned {
    pub pty: Pty,
    /// What the program writes, for a reader thread. Ends once the program
    /// and everything it started have exited on Unix, and once the [Pty] is
    /// dropped on Windows, where the pseudo console keeps it open until then.
    pub output: File,
    pub child: Child,
}

/// Start `program` with `args` in `dir`, in a new pseudo terminal of `cols`
/// by `rows` characters.
pub fn spawn(
    program: &str,
    args: &[&str],
    dir: &Path,
    cols: u16,
    rows: u16,
) -> io::Result<Spawned> {
    let (cols, rows) = (cols.max(1), rows.max(1));
    let (sys, input, output, child) = sys::spawn(program, args, dir, cols, rows)?;
    Ok(Spawned {
        pty: Pty {
            input,
            size: (cols, rows),
            sys,
        },
        output,
        child: Child(child),
    })
}

impl Pty {
    /// Send bytes to the program, as typed. Never blocks for long: the
    /// terminal takes input whether or not the program is reading it.
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.input.write_all(bytes)?;
        self.input.flush()
    }

    /// Ctrl+C, as a terminal sends it: the terminal interrupts the program.
    pub fn interrupt(&mut self) -> io::Result<()> {
        self.send(b"\x03")
    }

    /// The end of the input, as typed at the start of a line: Ctrl+D on
    /// Unix, Ctrl+Z then Enter on Windows.
    pub fn end_input(&mut self) -> io::Result<()> {
        self.send(if cfg!(windows) { b"\x1a\r" } else { b"\x04" })
    }

    /// Change the terminal's size, so a full-screen program redraws to fit.
    /// A no-op when unchanged.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        self.sys.resize(cols, rows);
    }

    /// End the program and everything it started, without the chance to
    /// clean up that an [Self::interrupt] gives it.
    pub fn kill(&self) {
        self.sys.kill();
    }
}

impl Child {
    /// Wait for the program to exit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.0.wait()
    }
}

#[cfg(windows)]
mod sys {
    use std::{
        ffi::c_void,
        fs::File,
        io,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::ExitStatusExt,
        },
        path::Path,
        process::{ExitStatus, Stdio},
        ptr::{null, null_mut},
        thread,
    };

    use shell::quiet_command;
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED},
        System::{
            Console::{
                COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
                SetConsoleCtrlHandler,
            },
            JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject},
            Pipes::CreatePipe,
            Threading::{
                CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
                EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
                InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
                UpdateProcThreadAttribute, WaitForSingleObject,
            },
        },
    };

    pub struct Pty {
        con: HPCON,
        /// A job object holding the program and every process it starts, so
        /// [Self::kill] can end those rather than just the program. Closing it
        /// leaves them running: a program the command started on its own,
        /// such as an editor, should outlive it.
        job: Option<OwnedHandle>,
        pid: u32,
    }

    pub struct Child {
        process: OwnedHandle,
    }

    fn coord(cols: u16, rows: u16) -> COORD {
        COORD {
            X: cols.min(i16::MAX as u16) as i16,
            Y: rows.min(i16::MAX as u16) as i16,
        }
    }

    fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let (mut read, mut write) = (null_mut(), null_mut());
        // SAFETY: both out-pointers are valid; the handles are ours to own.
        unsafe {
            if CreatePipe(&mut read, &mut write, null(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((
                OwnedHandle::from_raw_handle(read),
                OwnedHandle::from_raw_handle(write),
            ))
        }
    }

    /// `program` and `args` as one command line, quoted as the C runtime
    /// splits it back up (and as `std::process::Command` quotes it).
    fn command_line(program: &str, args: &[&str]) -> Vec<u16> {
        let mut line = String::new();
        for (i, arg) in std::iter::once(&program).chain(args).enumerate() {
            if i > 0 {
                line.push(' ');
            }
            let quote = arg.is_empty() || arg.contains([' ', '\t']);
            if quote {
                line.push('"');
            }
            let mut backslashes = 0;
            for c in arg.chars() {
                if c == '\\' {
                    backslashes += 1;
                } else {
                    if c == '"' {
                        // 2n + 1 backslashes before a quote inside the arg.
                        line.extend(std::iter::repeat_n('\\', backslashes + 1));
                    }
                    backslashes = 0;
                }
                line.push(c);
            }
            if quote {
                // 2n before the closing quote.
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push('"');
            }
        }
        line.encode_utf16().chain([0]).collect()
    }

    pub fn spawn(
        program: &str,
        args: &[&str],
        dir: &Path,
        cols: u16,
        rows: u16,
    ) -> io::Result<(Pty, File, File, Child)> {
        // The pseudo console reads the program's input from one pipe and
        // writes what it draws to the other. It keeps its own copies of its
        // ends, so ours close here.
        let (con_in, input) = pipe()?;
        let (output, con_out) = pipe()?;
        let mut con: HPCON = 0;
        // SAFETY: valid pipe handles and out-pointer.
        let hr = unsafe {
            CreatePseudoConsole(
                coord(cols, rows),
                con_in.as_raw_handle(),
                con_out.as_raw_handle(),
                0,
                &mut con,
            )
        };
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        drop((con_in, con_out));
        let mut pty = Pty {
            con,
            job: None,
            pid: 0,
        };

        // Whether a process ignores Ctrl+C passes down to the processes it
        // starts, and whatever started this one may have had it ignored (as
        // some IDEs and launchers do), which would leave Ctrl+C unable to
        // interrupt anything run here. Taking it back costs nothing: a
        // release build has no console of its own to take Ctrl+C from, and
        // in a debug one, Ctrl+C ending it is the default anyway.
        // SAFETY: a plain call; no handler involved.
        unsafe { SetConsoleCtrlHandler(None, 0) };

        let mut line = command_line(program, args);
        let dir: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: the attribute list is sized as Windows asks, kept alive and
        // in place until the process is created, then deleted; every
        // pointer passed points at a live local.
        let info = unsafe {
            let mut size = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
            // `usize`s, for the list's alignment.
            let mut list = vec![0usize; size.div_ceil(size_of::<usize>())];
            let attrs = list.as_mut_ptr().cast::<c_void>();
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) == 0 {
                return Err(io::Error::last_os_error());
            }
            let attached = UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                con as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            );
            if attached == 0 {
                let e = io::Error::last_os_error();
                DeleteProcThreadAttributeList(attrs);
                return Err(e);
            }

            let mut startup: STARTUPINFOEXW = zeroed();
            startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            startup.lpAttributeList = attrs;
            // Explicitly none, so the program takes the pseudo console's,
            // rather than inheriting this process's own (a log file, or the
            // console of a debug build).
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
            startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

            let mut info: PROCESS_INFORMATION = zeroed();
            // Suspended until it's in the job, so nothing it starts escapes.
            let created = CreateProcessW(
                null(),
                line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED,
                null(),
                dir.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            );
            let e = io::Error::last_os_error();
            DeleteProcThreadAttributeList(attrs);
            if created == 0 {
                return Err(e);
            }
            info
        };

        // SAFETY: handles CreateProcessW just gave us, each owned once.
        let (process, thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(info.hProcess),
                OwnedHandle::from_raw_handle(info.hThread),
            )
        };
        pty.pid = info.dwProcessId;
        pty.job = job_for(&process);
        // SAFETY: the main thread's handle, still open.
        unsafe { ResumeThread(thread.as_raw_handle()) };

        Ok((
            pty,
            File::from(input),
            File::from(output),
            Child { process },
        ))
    }

    /// A new job holding `process`, or `None` if Windows refused one.
    fn job_for(process: &OwnedHandle) -> Option<OwnedHandle> {
        // SAFETY: plain calls on handles we own.
        unsafe {
            let job = CreateJobObjectW(null(), null());
            if job.is_null() {
                return None;
            }
            let job = OwnedHandle::from_raw_handle(job);
            (AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) != 0)
                .then_some(job)
        }
    }

    impl Pty {
        pub fn resize(&self, cols: u16, rows: u16) {
            // SAFETY: `self.con` is open until drop.
            unsafe { ResizePseudoConsole(self.con, coord(cols, rows)) };
        }

        pub fn kill(&self) {
            // SAFETY: a job handle we own.
            let ended = self
                .job
                .as_ref()
                .is_some_and(|job| unsafe { TerminateJobObject(job.as_raw_handle(), 1) } != 0);
            if !ended {
                // No job, so end the program's process tree by walking it.
                let _ = quiet_command("taskkill")
                    .args(["/T", "/F", "/PID", &self.pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            // Closing waits for the pseudo console's last output to be read,
            // on some versions of Windows, so not on the UI thread.
            let con = self.con;
            // SAFETY: `con` is ours, and closed only here.
            thread::spawn(move || unsafe { ClosePseudoConsole(con) });
        }
    }

    impl Child {
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            let handle: HANDLE = self.process.as_raw_handle();
            let mut code = 0;
            // SAFETY: a process handle we own.
            unsafe {
                if WaitForSingleObject(handle, INFINITE) == WAIT_FAILED
                    || GetExitCodeProcess(handle, &mut code) == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(ExitStatus::from_raw(code))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::command_line;

        #[test]
        fn args_are_quoted_as_the_c_runtime_splits_them() {
            let line = command_line("pwsh", &["-Command", r#"echo "a b" C:\x\"#, "", r"C:\y"]);
            let line = String::from_utf16(&line[..line.len() - 1]).unwrap();
            assert_eq!(line, r#"pwsh -Command "echo \"a b\" C:\x\\" "" C:\y"#);
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::{
        fs::File,
        io,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::process::CommandExt,
        },
        path::Path,
        process::{Command, ExitStatus, Stdio},
        ptr::null_mut,
    };

    pub struct Pty {
        master: OwnedFd,
        /// The program's, which leads its own session and process group.
        pid: u32,
    }

    pub struct Child(std::process::Child);

    fn winsize(cols: u16, rows: u16) -> libc::winsize {
        libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }
    }

    fn cloexec(fd: &OwnedFd) -> io::Result<()> {
        // SAFETY: an open descriptor we own.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn spawn(
        program: &str,
        args: &[&str],
        dir: &Path,
        cols: u16,
        rows: u16,
    ) -> io::Result<(Pty, File, File, Child)> {
        let (mut master, mut slave) = (-1, -1);
        let mut size = winsize(cols, rows);
        // SAFETY: valid out-pointers; the descriptors are ours to own.
        let (master, slave) = unsafe {
            if libc::openpty(&mut master, &mut slave, null_mut(), null_mut(), &mut size) != 0 {
                return Err(io::Error::last_os_error());
            }
            (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
        };
        // The program gets the terminal as its stdio only.
        cloexec(&master)?;
        cloexec(&slave)?;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(dir)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // A session of its own, with the terminal as its controlling
                // one: Ctrl+C there interrupts it, and `kill` can end its
                // whole process group.
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // Its copies of the terminal end, or reading would never see the end.
        drop(cmd);

        let input = File::from(master.try_clone()?);
        let output = File::from(master.try_clone()?);
        let pid = child.id();
        Ok((Pty { master, pid }, input, output, Child(child)))
    }

    impl Pty {
        pub fn resize(&self, cols: u16, rows: u16) {
            let size = winsize(cols, rows);
            // SAFETY: an open descriptor and a valid `winsize`.
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
        }

        pub fn kill(&self) {
            // The whole process group: the program was started as its leader.
            // SAFETY: a plain signal.
            unsafe { libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL) };
        }
    }

    impl Child {
        pub fn wait(&mut self) -> io::Result<ExitStatus> {
            self.0.wait()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// A program in the terminal writes to it, and its exit status comes back.
    #[test]
    fn runs_a_program_in_a_terminal() {
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/c", "echo hello from the pty & exit 3"])
        } else {
            ("sh", &["-c", "echo hello from the pty; exit 3"])
        };
        let Spawned {
            pty,
            mut output,
            mut child,
        } = spawn(program, args, &std::env::temp_dir(), 80, 24).unwrap();

        let reader = std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = output.read(&mut buf) {
                seen.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&seen).contains("hello from the pty") {
                    break;
                }
            }
            String::from_utf8_lossy(&seen).into_owned()
        });
        assert_eq!(child.wait().unwrap().code(), Some(3));
        let seen = reader.join().unwrap();
        assert!(seen.contains("hello from the pty"), "{seen:?}");
        drop(pty);
    }

    /// Ctrl+C typed into the terminal interrupts the program.
    #[test]
    fn interrupts_a_program() {
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("ping", &["-n", "30", "127.0.0.1"])
        } else {
            ("sh", &["-c", "sleep 30"])
        };
        let Spawned {
            mut pty,
            mut output,
            mut child,
        } = spawn(program, args, &std::env::temp_dir(), 80, 24).unwrap();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(1..) = output.read(&mut buf) {}
        });
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(child.wait().unwrap()));

        std::thread::sleep(std::time::Duration::from_millis(500));
        pty.interrupt().unwrap();
        let status = rx.recv_timeout(std::time::Duration::from_secs(10));
        assert!(status.is_ok_and(|s| !s.success()), "{status:?}");
    }
}
