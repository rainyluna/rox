//! Spawning a plugin and killing it with everything it started. A plugin
//! runs with the user's permissions, so the one thing the host owes the user
//! is that switching it off, or quitting rox, leaves nothing of it running.
//!
//! Unix puts the plugin in its own process group and signals the group.
//! Windows assigns it to a job object that kills everything in it when the
//! handle closes; a grandchild spawned between the spawn and the assignment
//! escapes, and the contract accepts that.

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::search;

/// Without it every plugin start pops a console window.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Longest stderr line kept; the rest of the line is dropped.
const STDERR_LINE: usize = 4096;

pub struct Process {
    child: Child,
    #[cfg(windows)]
    job: Option<job::Job>,
}

pub struct Pipes {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

/// Starts `command` in the plugin folder with its pipes attached. The
/// plugin inherits rox's environment (HOME for its programs' caches) with
/// PATH replaced by the plugin's search path, so what it runs resolves the
/// way the Plugins page reported it, plus three variables for Python.
pub fn spawn(id: &str, mut command: Command, dir: &Path) -> Result<(Process, Pipes), String> {
    command
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PATH", search::search_path(Some(dir)))
        // A Python plugin's first import would write `__pycache__` into its
        // own folder and change its own hash.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        // A piped stdout is block-buffered, so answers would sit unsent.
        .env("PYTHONUNBUFFERED", "1")
        // Windows reads piped text in the ANSI code page, not UTF-8.
        .env("PYTHONUTF8", "1");

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;

    #[cfg(windows)]
    let job = match job::Job::adopt(&child) {
        Ok(job) => Some(job),
        Err(e) => {
            log::warn!("plugin {id}: no job object, its children may outlive it: {e}");
            None
        }
    };

    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        return Err("could not attach to the plugin's pipes".into());
    };

    let tag = id.to_string();
    let drained = std::thread::Builder::new()
        .name(format!("plugin-{id}-stderr"))
        .spawn(move || drain_stderr(&tag, stderr));
    if let Err(e) = drained {
        log::warn!("plugin {id}: stderr goes unread: {e}");
    }

    let process = Process {
        child,
        #[cfg(windows)]
        job,
    };

    Ok((process, Pipes { stdin, stdout }))
}

fn drain_stderr(id: &str, stderr: impl Read) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();

    loop {
        match read_line(&mut reader, &mut line, STDERR_LINE) {
            Ok(Line::End) | Err(_) => return,

            Ok(Line::Whole | Line::Clipped) => {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end();
                if !text.is_empty() {
                    log::info!("plugin {id}: {text}");
                }
            }
        }
    }
}

pub enum Line {
    Whole,
    /// Longer than the cap; `buf` holds the head and the rest was skipped.
    Clipped,
    End,
}

/// One line into `buf`, newline stripped, never holding more than `cap`
/// bytes however long the line runs.
pub fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> io::Result<Line> {
    buf.clear();
    let mut clipped = false;

    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };

        if available.is_empty() {
            return Ok(match (buf.is_empty() && !clipped, clipped) {
                (true, _) => Line::End,
                (false, true) => Line::Clipped,
                (false, false) => Line::Whole,
            });
        }

        let (take, newline) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (at, true),
            None => (available.len(), false),
        };

        let room = cap.saturating_sub(buf.len());
        if take > room {
            clipped = true;
        }
        buf.extend_from_slice(&available[..take.min(room)]);
        reader.consume(take + usize::from(newline));

        if newline {
            return Ok(match clipped {
                true => Line::Clipped,
                false => Line::Whole,
            });
        }
    }
}

impl Process {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// True once it's gone, false if it's still running at the deadline.
    pub fn wait_for(&mut self, within: Duration) -> bool {
        let deadline = Instant::now() + within;

        loop {
            if self.exited().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }

            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The plugin and everything it started. Safe to call twice.
    pub fn kill(&mut self) {
        #[cfg(unix)]
        {
            // `process_group(0)` made the plugin its group's leader, so its pid
            // is the group id. A child that re-grouped itself escapes, as it
            // would from any shell.
            let group = self.child.id() as libc::pid_t;

            // SAFETY: kill(2) on our own child's group; no memory is shared.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }

        #[cfg(windows)]
        {
            // Closing the last handle kills every process in the job.
            self.job.take();
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel object reference, usable from any thread.
    unsafe impl Send for Job {}

    impl Job {
        pub fn adopt(child: &Child) -> Result<Job, String> {
            // SAFETY: plain Win32 calls on handles this function owns or borrows
            // from `child` for the duration of the call.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let job = Job(handle);

                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }

                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }

                Ok(job)
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(input: &[u8], cap: usize) -> Vec<(String, bool)> {
        let mut reader = BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let mut out = Vec::new();

        loop {
            match read_line(&mut reader, &mut buf, cap).unwrap() {
                Line::End => return out,
                Line::Whole => out.push((String::from_utf8_lossy(&buf).into_owned(), false)),
                Line::Clipped => out.push((String::from_utf8_lossy(&buf).into_owned(), true)),
            }
        }
    }

    #[test]
    fn lines_split_on_newlines_across_small_buffers() {
        assert_eq!(
            lines(b"one\ntwo three\n\nlast", 64),
            vec![
                ("one".into(), false),
                ("two three".into(), false),
                ("".into(), false),
                ("last".into(), false),
            ]
        );
    }

    #[test]
    fn a_long_line_is_clipped_and_the_next_one_still_reads() {
        assert_eq!(
            lines(b"abcdefghij\nok\n", 4),
            vec![("abcd".into(), true), ("ok".into(), false)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_plugin_gets_the_python_variables_and_its_bin_first_on_path() {
        let dir = std::env::temp_dir().join(format!("rox-plugins-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(r#"printf '%s\n%s\n%s\n' "$PYTHONUNBUFFERED" "$PYTHONUTF8" "$PATH""#);
        let (mut process, pipes) = spawn("env", command, &dir).unwrap();

        let mut out = String::new();
        let mut stdout = pipes.stdout;
        stdout.read_to_string(&mut out).unwrap();
        process.wait_for(Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);

        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[..2], ["1", "1"], "{out}");

        let first = std::env::split_paths(lines[2]).next();
        assert_eq!(first, Some(dir.join("bin")), "{out}");
    }
}
