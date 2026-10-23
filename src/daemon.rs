//! Daemon control: `start` spawns a detached `serve` child process, while
//! `stop` and `status` drive it through a pid file that lives next to the
//! config file (`config.yaml` -> `config.pid`, with logs in `config.log`).
//!
//! The child gets its own process group (`process_group(0)`) so terminal
//! Ctrl-C and hangup never reach it; an explicit `kill -HUP <pid>` still
//! reloads the config.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

/// How long `start` waits for the child to bind and write its pid file, and
/// how long `stop` waits for SIGTERM to take effect.
const WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub fn pid_file_for(config_path: &str) -> PathBuf {
    let mut path = PathBuf::from(config_path);
    path.set_extension("pid");
    path
}

pub fn log_file_for(config_path: &str) -> PathBuf {
    let mut path = PathBuf::from(config_path);
    path.set_extension("log");
    path
}

pub fn read_pid(path: &Path) -> Option<i32> {
    let text = fs::read_to_string(path).ok()?;
    let pid: i32 = text.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

pub fn process_alive(pid: i32) -> bool {
    // kill(pid, 0) only probes: success or EPERM means the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Spawn `serve` as a detached background process. `serve_args` carries the
/// flags (`--host`, `--log-level`, `--registry-protocols`) forwarded verbatim.
pub fn start(config_path: &str, serve_args: &[String]) -> ExitCode {
    let pid_file = pid_file_for(config_path);
    let log_file = log_file_for(config_path);

    if let Some(pid) = read_pid(&pid_file) {
        if process_alive(pid) {
            eprintln!("already running (pid {pid})");
            return ExitCode::from(1);
        }
        // Left over from an unclean exit (kill -9, crash).
        let _ = fs::remove_file(&pid_file);
    }

    // First run: the config's directory may not exist yet, and the log file
    // has to be opened before the child can create anything.
    if let Some(dir) = log_file.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        if let Err(error) = fs::create_dir_all(dir) {
            eprintln!("cannot create directory {}: {error}", dir.display());
            return ExitCode::from(1);
        }
    }

    let log = match OpenOptions::new().create(true).append(true).open(&log_file) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("cannot open log file {}: {error}", log_file.display());
            return ExitCode::from(1);
        }
    };
    let log_err = match log.try_clone() {
        Ok(file) => file,
        Err(error) => {
            eprintln!("cannot open log file {}: {error}", log_file.display());
            return ExitCode::from(1);
        }
    };

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            eprintln!("cannot locate the current executable: {error}");
            return ExitCode::from(1);
        }
    };
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve")
        .arg("--config")
        .arg(config_path)
        .arg("--pid-file")
        .arg(&pid_file)
        .args(serve_args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        // Detach from the terminal's foreground process group so Ctrl-C and
        // terminal hangup never reach the daemon (setpgid, not setsid: the
        // stdio redirection above makes the session/controlling-tty difference
        // irrelevant here, and this std API is safe).
        .process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("cannot spawn the daemon process: {error}");
            return ExitCode::from(1);
        }
    };

    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        // The child writes its pid file only after the socket is bound, so
        // its appearance doubles as the readiness signal.
        if let Some(pid) = read_pid(&pid_file) {
            println!("started (pid {pid})\n  logs: {}", log_file.display());
            return ExitCode::SUCCESS;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                eprintln!("daemon exited during startup ({status}); last log lines:");
                eprint!("{}", tail(&log_file, 4096));
                return ExitCode::from(1);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("cannot monitor the daemon process: {error}");
                return ExitCode::from(1);
            }
        }
        if Instant::now() >= deadline {
            eprintln!(
                "daemon did not write its pid file within {}s; see {}",
                WAIT_TIMEOUT.as_secs(),
                log_file.display()
            );
            return ExitCode::from(1);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// SIGTERM the daemon recorded in the pid file. Idempotent: a missing pid
/// file or a dead process counts as "not running", not an error.
pub fn stop(config_path: &str) -> ExitCode {
    let pid_file = pid_file_for(config_path);
    let Some(pid) = read_pid(&pid_file) else {
        println!("not running");
        return ExitCode::SUCCESS;
    };
    if !process_alive(pid) {
        let _ = fs::remove_file(&pid_file);
        println!("not running (removed stale pid file)");
        return ExitCode::SUCCESS;
    }
    let signaled = unsafe { libc::kill(pid, libc::SIGTERM) };
    if signaled < 0 {
        eprintln!(
            "cannot signal pid {pid}: {}",
            std::io::Error::last_os_error()
        );
        return ExitCode::from(1);
    }
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
    if process_alive(pid) {
        eprintln!(
            "pid {pid} is still alive {}s after SIGTERM; not forcing it down",
            WAIT_TIMEOUT.as_secs()
        );
        return ExitCode::from(1);
    }
    // The child removes the pid file itself on graceful shutdown; this only
    // covers the hard-exit fallback path.
    let _ = fs::remove_file(&pid_file);
    println!("stopped (pid {pid})");
    ExitCode::SUCCESS
}

/// SIGHUP the daemon recorded in the pid file, asking it to reload the config.
/// Only daemons launched with `start` have a pid file; a foreground `serve`
/// takes `kill -HUP <pid>` directly.
pub fn reload(config_path: &str) -> ExitCode {
    let pid_file = pid_file_for(config_path);
    let Some(pid) = read_pid(&pid_file) else {
        eprintln!("not running (no pid file; is the bridge running under `start`?)");
        return ExitCode::from(1);
    };
    if !process_alive(pid) {
        let _ = fs::remove_file(&pid_file);
        eprintln!("not running (removed stale pid file)");
        return ExitCode::from(1);
    }
    let signaled = unsafe { libc::kill(pid, libc::SIGHUP) };
    if signaled != 0 {
        eprintln!(
            "cannot signal pid {pid}: {}",
            std::io::Error::last_os_error()
        );
        return ExitCode::from(1);
    }
    println!("reloading (pid {pid})");
    ExitCode::SUCCESS
}

pub fn status(config_path: &str) -> ExitCode {
    match read_pid(&pid_file_for(config_path)) {
        Some(pid) if process_alive(pid) => {
            println!("running (pid {pid})");
            println!("  logs: {}", log_file_for(config_path).display());
            ExitCode::SUCCESS
        }
        _ => {
            println!("not running");
            ExitCode::from(1)
        }
    }
}

fn tail(path: &Path, max_bytes: u64) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(max_bytes)));
    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_and_log_files_replace_the_config_extension() {
        assert_eq!(pid_file_for("config.yaml"), PathBuf::from("config.pid"));
        assert_eq!(log_file_for("config.yaml"), PathBuf::from("config.log"));
        assert_eq!(
            pid_file_for("/etc/ocb/prod.yaml"),
            PathBuf::from("/etc/ocb/prod.pid")
        );
        assert_eq!(
            log_file_for("/etc/ocb/prod.yaml"),
            PathBuf::from("/etc/ocb/prod.log")
        );
        assert_eq!(pid_file_for("noext"), PathBuf::from("noext.pid"));
    }

    #[test]
    fn read_pid_rejects_garbage() {
        let path = std::env::temp_dir().join(format!(
            "opencode-bridge-read-pid-{}.pid",
            std::process::id()
        ));
        assert_eq!(read_pid(&path), None);
        fs::write(&path, "not a pid").unwrap();
        assert_eq!(read_pid(&path), None);
        fs::write(&path, " -3\n").unwrap();
        assert_eq!(read_pid(&path), None);
        let own = std::process::id() as i32;
        fs::write(&path, format!("{own}\n")).unwrap();
        assert_eq!(read_pid(&path), Some(own));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn process_alive_checks() {
        assert!(process_alive(std::process::id() as i32));
        // Far above any realistic pid_max, so the probe answers ESRCH.
        assert!(!process_alive(i32::MAX));
    }
}
