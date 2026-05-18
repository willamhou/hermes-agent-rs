//! Background process registry — spawn, track, poll, and kill background commands.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, LazyLock, Mutex},
};

use tokio::io::AsyncBufReadExt;

/// Maximum output buffer size per process (200 KB).
const MAX_BUFFER_BYTES: usize = 200 * 1024;

/// Find the smallest byte index >= `index` that is a valid UTF-8 char boundary.
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

static REGISTRY: LazyLock<ProcessRegistry> = LazyLock::new(ProcessRegistry::new);

/// Get the global process registry.
pub fn global_registry() -> &'static ProcessRegistry {
    &REGISTRY
}

// ─── Types ───────────────────────────────────────────────────────────────────

/// Summary info returned by `list()`.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub id: String,
    pub command: String,
    pub started_at: String,
    pub status: ProcessStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited(i32),
}

impl std::fmt::Display for ProcessStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessStatus::Running => write!(f, "running"),
            ProcessStatus::Exited(code) => write!(f, "exited({code})"),
        }
    }
}

/// Maximum concurrent background processes.
const MAX_PROCESSES: usize = 32;

/// Internal entry for a tracked process.
struct ProcessEntry {
    command: String,
    started_at: String,
    pid: u32,
    process_group: Option<u32>,
    output: Arc<Mutex<String>>,
    exit_code: Arc<Mutex<Option<i32>>>,
}

#[cfg(unix)]
pub(crate) fn configure_child_process_group(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn configure_child_process_group(_cmd: &mut tokio::process::Command) {}

pub(crate) fn kill_process(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("unknown PID".to_string());
    }

    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if ret == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("kill failed: {err}"))
    }
}

pub(crate) fn kill_process_group(process_group: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        if process_group == 0 {
            return Err("unknown process group".to_string());
        }

        let ret = unsafe { libc::kill(-(process_group as libc::pid_t), libc::SIGKILL) };
        if ret == 0 {
            return Ok(());
        }

        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(format!("killpg failed: {err}"))
        }
    }

    #[cfg(not(unix))]
    {
        kill_process(process_group)
    }
}

// ─── Registry ────────────────────────────────────────────────────────────────

pub struct ProcessRegistry {
    processes: Mutex<HashMap<String, ProcessEntry>>,
}

impl ProcessRegistry {
    fn new() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a command in the background. Returns the process ID.
    pub fn spawn(&self, command: &str, workdir: &Path) -> Result<String, String> {
        // Auto-cleanup exited processes before checking limits
        self.remove_exited();

        // Check process limit
        {
            let guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
            let running = guard.len();
            if running >= MAX_PROCESSES {
                return Err(format!("process limit reached ({MAX_PROCESSES} running)"));
            }
        }

        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();

        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", command]);
        cmd.current_dir(workdir);
        cmd.stdout(std::process::Stdio::piped());
        // Merge stderr into stdout to avoid interleaved output
        cmd.stderr(std::process::Stdio::piped());
        configure_child_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
        let pid = child.id().unwrap_or(0);

        let output = Arc::new(Mutex::new(String::new()));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

        // Read stdout into buffer
        let out_buf = Arc::clone(&output);
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = out_buf.lock().unwrap_or_else(|e| e.into_inner());
                    buf.push_str(&line);
                    buf.push('\n');
                    if buf.len() > MAX_BUFFER_BYTES {
                        let drop_at = buf.len() - MAX_BUFFER_BYTES;
                        let boundary = ceil_char_boundary(&buf, drop_at);
                        *buf = buf[boundary..].to_string();
                    }
                }
            });
        }

        // Read stderr into same buffer (separate task to avoid blocking)
        let err_buf = Arc::clone(&output);
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = err_buf.lock().unwrap_or_else(|e| e.into_inner());
                    buf.push_str("[stderr] ");
                    buf.push_str(&line);
                    buf.push('\n');
                    if buf.len() > MAX_BUFFER_BYTES {
                        let drop_at = buf.len() - MAX_BUFFER_BYTES;
                        let boundary = ceil_char_boundary(&buf, drop_at);
                        *buf = buf[boundary..].to_string();
                    }
                }
            });
        }

        // Wait for exit code
        let exit_ref = Arc::clone(&exit_code);
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    let code = status.code().unwrap_or(-1);
                    if let Ok(mut guard) = exit_ref.lock() {
                        *guard = Some(code);
                    }
                }
                Err(e) => {
                    tracing::warn!("background process wait error: {e}");
                    if let Ok(mut guard) = exit_ref.lock() {
                        *guard = Some(-1);
                    }
                }
            }
        });

        let entry = ProcessEntry {
            command: command.to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            pid,
            process_group: (pid != 0).then_some(pid),
            output,
            exit_code,
        };

        self.processes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), entry);

        Ok(id)
    }

    /// List all tracked processes.
    pub fn list(&self) -> Vec<ProcessInfo> {
        let guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        let mut infos: Vec<ProcessInfo> = guard
            .iter()
            .map(|(id, entry)| {
                let status = match entry.exit_code.lock().ok().and_then(|g| *g) {
                    Some(code) => ProcessStatus::Exited(code),
                    None => ProcessStatus::Running,
                };
                let cmd_display = if entry.command.chars().count() > 60 {
                    let truncated: String = entry.command.chars().take(57).collect();
                    format!("{truncated}...")
                } else {
                    entry.command.clone()
                };
                ProcessInfo {
                    id: id.clone(),
                    command: cmd_display,
                    started_at: entry.started_at.clone(),
                    status,
                }
            })
            .collect();
        infos.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        infos
    }

    /// Read the output buffer of a process.
    pub fn read_output(&self, id: &str) -> Option<String> {
        let guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(id).map(|entry| {
            entry
                .output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
    }

    /// Kill a background process by sending SIGKILL to its process group when available.
    pub fn kill(&self, id: &str) -> Result<(), String> {
        let guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        let entry = guard
            .get(id)
            .ok_or_else(|| format!("process {id} not found"))?;
        if entry.exit_code.lock().ok().and_then(|g| *g).is_some() {
            return Err(format!("process {id} already exited"));
        }
        if let Some(process_group) = entry.process_group {
            kill_process_group(process_group)
        } else {
            kill_process(entry.pid)
        }
    }

    /// Check if a process is still running.
    pub fn is_running(&self, id: &str) -> bool {
        let guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get(id)
            .map(|e| e.exit_code.lock().ok().and_then(|g| *g).is_none())
            .unwrap_or(false)
    }

    /// Return the OS process-group ID for a tracked background process when known.
    pub fn process_group_for(&self, id: &str) -> Option<u32> {
        let guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(id).and_then(|entry| entry.process_group)
    }

    /// Remove all exited processes.
    pub fn remove_exited(&self) {
        let mut guard = self.processes.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, entry| entry.exit_code.lock().ok().and_then(|g| *g).is_none());
    }

    /// Number of tracked processes.
    pub fn len(&self) -> usize {
        self.processes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// True if no processes are tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use tokio::time::Duration;

    fn pid_is_alive(pid: u32) -> bool {
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }

    async fn wait_for_pid_file(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    fn test_registry() -> ProcessRegistry {
        ProcessRegistry::new()
    }

    #[tokio::test]
    async fn spawn_and_list() {
        let reg = test_registry();
        let id = reg.spawn("echo hello", Path::new("/tmp")).unwrap();
        assert_eq!(id.len(), 8);

        let list = reg.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert!(list[0].command.contains("echo hello"));
    }

    #[tokio::test]
    async fn read_output() {
        let reg = test_registry();
        let id = reg.spawn("echo hello_bg", Path::new("/tmp")).unwrap();

        // Wait for process to finish and output to be captured
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let output = reg.read_output(&id).unwrap();
        assert!(output.contains("hello_bg"), "output was: {output}");
    }

    #[tokio::test]
    async fn exit_code_captured() {
        let reg = test_registry();
        let id = reg.spawn("exit 42", Path::new("/tmp")).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert!(!reg.is_running(&id));
        let infos = reg.list();
        assert_eq!(infos[0].status, ProcessStatus::Exited(42));
    }

    #[tokio::test]
    async fn remove_exited() {
        let reg = test_registry();
        reg.spawn("echo done", Path::new("/tmp")).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_eq!(reg.len(), 1);
        reg.remove_exited();
        assert_eq!(reg.len(), 0);
    }

    #[tokio::test]
    async fn kill_terminates_background_process_group_descendants() {
        let reg = test_registry();
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && wait", pid_file.path().display());

        let id = reg.spawn(&command, Path::new("/tmp")).unwrap();
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));

        reg.kill(&id).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            !pid_is_alive(descendant_pid),
            "descendant pid {descendant_pid} should be terminated with the shell process group"
        );
    }

    #[test]
    fn nonexistent_process() {
        let reg = test_registry();
        assert!(reg.read_output("nope").is_none());
        assert!(!reg.is_running("nope"));
    }
}
