//! Product CLI surface for the session-monitor host.
//!
//! Bake/status/disable wrap the existing Kakao-blind packager. `--tick` is the
//! LaunchAgent one-shot: it may open Terminal on a digest-pinned `.command`
//! and never reads KakaoTalk, AX, or the reply queue.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::{self, OpenKakaoConfig};

const MONITOR_LABEL: &str = "com.openkakao.auto-reply.session-monitor";
const STATUS_SCHEMA: u32 = 1;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const WINDOW_NS: i128 = 15 * 60 * 1_000_000_000;
const MAX_LAUNCHES_PER_WINDOW: usize = 3;
const LAUNCH_COOLDOWN_NS: i128 = 60 * 1_000_000_000;
const BACKGROUND_OPEN_ARGV: [&str; 6] = [
    "/usr/bin/open",
    "-g",
    "-j",
    "--hide",
    "-b",
    "com.apple.Terminal",
];
const WATCHDOG_WINDOW_SCRIPT: &str = r#"tell application "Terminal"
repeat with w in (get windows)
try
set wn to name of w as text
if wn contains "start-auto-reply-session.command" then
if (busy of w) is false then
close w saving no
else
set miniaturized of w to true
end if
end if
end try
end repeat
end tell"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReplyHostAction {
    Status,
    Bake,
    Disable,
    Tick,
}

pub struct AutoReplyHostOptions {
    pub action: AutoReplyHostAction,
    pub chats: Vec<String>,
    pub json: bool,
    pub manifest: Option<PathBuf>,
    pub state_root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct MonitorManifest {
    schema_version: u32,
    state_root: String,
    command: MonitorCommand,
}

#[derive(Debug, Deserialize)]
struct MonitorCommand {
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct MonitorStatus {
    #[serde(default)]
    launches_unix_ns: Vec<i128>,
    #[serde(default)]
    next_attempt_at_unix_ns: i128,
}

fn repo_root() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolve openkakao-cli path")?;
    let mut cursor = exe.parent();
    while let Some(dir) = cursor {
        if dir
            .join("scripts/prepare-auto-reply-session-runtime.py")
            .is_file()
            && dir.join("Cargo.toml").is_file()
        {
            return Ok(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    bail!(
        "could not find the openkakao-cli repository root from {}",
        exe.display()
    );
}

fn script_path(name: &str) -> Result<PathBuf> {
    let path = repo_root()?.join("scripts").join(name);
    if !path.is_file() {
        bail!("missing host script {}", path.display());
    }
    Ok(path)
}

fn configured_python(config: &OpenKakaoConfig) -> Result<PathBuf> {
    let raw = config
        .auto_reply
        .python_interpreter
        .as_deref()
        .unwrap_or("/opt/homebrew/opt/python@3.11/bin/python3.11");
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        bail!("auto_reply.python_interpreter must be an absolute path");
    }
    if path
        .to_str()
        .is_some_and(|value| value.contains("/Cellar/python@"))
    {
        bail!("auto_reply.python_interpreter must be the Homebrew keg python@3.11 path, not a Cellar version");
    }
    Ok(path)
}

fn configured_chats(config: &OpenKakaoConfig, override_chats: &[String]) -> Result<Vec<String>> {
    let chats = if override_chats.is_empty() {
        config.auto_reply.chats.clone()
    } else {
        override_chats.to_vec()
    };
    if chats.is_empty() {
        bail!("auto-reply-host needs --chat or [auto_reply].chats");
    }
    Ok(chats)
}

fn run_script(program: &Path, args: &[&str]) -> Result<(i32, String, String)> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {}", program.display()))?;
    Ok((
        output.status.code().unwrap_or(1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn emit(
    json_out: bool,
    payload: serde_json::Value,
    stdout: &str,
    stderr: &str,
    code: i32,
) -> Result<()> {
    if json_out {
        crate::util::output_json(&payload)?;
    } else {
        if !stdout.is_empty() {
            print!("{stdout}");
        }
        if !stderr.is_empty() {
            eprint!("{stderr}");
        }
    }
    if code != 0 {
        bail!("auto-reply-host helper exited {code}");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("hash {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn now_unix_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

fn require_private_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() || path.is_symlink() {
        bail!("state root must be an absolute non-symlink path");
    }
    let resolved = path.canonicalize().context("resolve state root")?;
    let metadata = fs::metadata(&resolved)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        bail!("state root must be private and user-owned");
    }
    Ok(resolved)
}

fn require_owned_file(path: &Path, exact_mode: Option<u32>) -> Result<PathBuf> {
    if !path.is_absolute() || path.is_symlink() {
        bail!("managed file path is unsafe");
    }
    let resolved = path.canonicalize().context("resolve managed file")?;
    let metadata = fs::metadata(&resolved)?;
    let mode = metadata.mode() & 0o777;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || mode & 0o022 != 0
        || exact_mode.is_some_and(|expected| mode != expected)
    {
        bail!("managed file ownership or mode is unsafe");
    }
    Ok(resolved)
}

fn is_relative_to(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

fn try_exclusive_lock(file: &File) -> Result<bool> {
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(err).context("flock")
}

fn open_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open lock {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        bail!("session monitor lock is unsafe");
    }
    Ok(file)
}

fn write_status(path: &Path, value: &serde_json::Value) -> Result<()> {
    let parent = path.parent().context("status parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("status tempfile")?;
    serde_json::to_writer(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o600))?;
    temporary
        .persist(path)
        .map_err(|error| anyhow::anyhow!("persist status: {error}"))?;
    Ok(())
}

fn status_payload(
    state: &str,
    reason: &str,
    launches: &[i128],
    next_attempt: i128,
    timestamp: i128,
    digest: &str,
) -> serde_json::Value {
    json!({
        "schema_version": STATUS_SCHEMA,
        "state": state,
        "reason": reason,
        "launches_unix_ns": launches,
        "next_attempt_at_unix_ns": next_attempt,
        "updated_at_unix_ns": timestamp,
        "command_sha256": digest,
    })
}

fn previous_launches(path: &Path, now_ns: i128) -> Result<(Vec<i128>, i128)> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let owned = require_owned_file(path, Some(0o600))?;
    let metadata = fs::metadata(&owned)?;
    if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
        bail!("session monitor status size is invalid");
    }
    let value: MonitorStatus = serde_json::from_str(&fs::read_to_string(&owned)?)
        .context("session monitor status is malformed")?;
    if value.launches_unix_ns.len() > MAX_LAUNCHES_PER_WINDOW
        || value.launches_unix_ns.iter().any(|item| *item <= 0)
        || value.next_attempt_at_unix_ns < 0
    {
        bail!("session monitor status is malformed");
    }
    let floor = now_ns - WINDOW_NS;
    let mut launches: Vec<i128> = value
        .launches_unix_ns
        .into_iter()
        .filter(|item| *item >= floor)
        .collect();
    launches.sort_unstable();
    Ok((launches, value.next_attempt_at_unix_ns))
}

fn load_tick_command(manifest: &Path, state_root: &Path) -> Result<(PathBuf, String)> {
    let manifest_path = require_owned_file(manifest, Some(0o600))?;
    if !is_relative_to(&manifest_path, state_root) {
        bail!("session monitor manifest must stay within the state root");
    }
    let raw = fs::read_to_string(&manifest_path)?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    let object = value
        .as_object()
        .context("session monitor manifest has the wrong shape")?;
    if object.keys().len() != 3
        || !object.contains_key("schema_version")
        || !object.contains_key("state_root")
        || !object.contains_key("command")
    {
        bail!("session monitor manifest has unknown fields");
    }
    let parsed: MonitorManifest = serde_json::from_value(value)?;
    if parsed.schema_version != 1 || parsed.state_root != state_root.to_string_lossy() {
        bail!("session monitor manifest identity is invalid");
    }
    if parsed.sha256_invalid() {
        bail!("session monitor command identity is invalid");
    }
    let command_path = require_owned_file(Path::new(&parsed.command.path), Some(0o500))?;
    if !is_relative_to(&command_path, state_root) {
        bail!("session monitor command must stay within the state root");
    }
    let digest = sha256_file(&command_path)?;
    if digest != parsed.command.sha256 {
        bail!("session monitor command digest changed");
    }
    Ok((command_path, digest))
}

impl MonitorManifest {
    fn sha256_invalid(&self) -> bool {
        self.command.sha256.len() != 64
            || !self
                .command
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }
}

fn bounded_output(bytes: &[u8]) -> bool {
    bytes.len() as u64 <= MAX_FILE_BYTES
}

fn run_tick(manifest: &Path, state_root: &Path) -> Result<i32> {
    let state_root = require_private_root(state_root)?;
    let (command_path, digest) = load_tick_command(manifest, &state_root)?;
    let status_path = state_root.join("session-monitor-status.json");
    let timestamp = now_unix_ns();
    let monitor_lock = open_lock(&state_root.join("session-monitor.lock"))?;
    if !try_exclusive_lock(&monitor_lock)? {
        return Ok(0);
    }

    let disabled = state_root.join("session-monitor.disabled");
    match disabled.symlink_metadata() {
        Ok(_) => {
            require_owned_file(&disabled, Some(0o600))?;
            write_status(
                &status_path,
                &status_payload("disabled", "disable_sentinel", &[], 0, timestamp, &digest),
            )?;
            return Ok(0);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect disable sentinel"),
    }

    {
        let watchdog_lock = open_lock(&state_root.join("session-watchdog.owner.lock"))?;
        if !try_exclusive_lock(&watchdog_lock)? {
            let (launches, next_attempt) = previous_launches(&status_path, timestamp)?;
            write_status(
                &status_path,
                &status_payload(
                    "watchdog_running",
                    "owner_lock_held",
                    &launches,
                    next_attempt,
                    timestamp,
                    &digest,
                ),
            )?;
            return Ok(0);
        }
    }
    {
        let supervisor_lock = open_lock(&state_root.join("supervisor.owner.lock"))?;
        if !try_exclusive_lock(&supervisor_lock)? {
            let (launches, next_attempt) = previous_launches(&status_path, timestamp)?;
            write_status(
                &status_path,
                &status_payload(
                    "orphan_owner_lock_held",
                    "supervisor_owner_lock_held",
                    &launches,
                    next_attempt,
                    timestamp,
                    &digest,
                ),
            )?;
            return Ok(0);
        }
    }

    let (mut launches, next_attempt) = previous_launches(&status_path, timestamp)?;
    if timestamp < next_attempt {
        let state = if launches.len() >= MAX_LAUNCHES_PER_WINDOW {
            "circuit_open"
        } else {
            "cooldown"
        };
        write_status(
            &status_path,
            &status_payload(
                state,
                "launch_rate_limited",
                &launches,
                next_attempt,
                timestamp,
                &digest,
            ),
        )?;
        return Ok(0);
    }
    if launches.len() >= MAX_LAUNCHES_PER_WINDOW {
        let next = launches[0] + WINDOW_NS;
        write_status(
            &status_path,
            &status_payload(
                "circuit_open",
                "launch_rate_limited",
                &launches,
                next,
                timestamp,
                &digest,
            ),
        )?;
        return Ok(0);
    }

    launches.push(timestamp);
    let next_attempt = timestamp + LAUNCH_COOLDOWN_NS;
    write_status(
        &status_path,
        &status_payload(
            "launching",
            "open_request_pending",
            &launches,
            next_attempt,
            timestamp,
            &digest,
        ),
    )?;

    let mut open = Command::new(BACKGROUND_OPEN_ARGV[0]);
    open.args(&BACKGROUND_OPEN_ARGV[1..])
        .arg(&command_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .env("HOME", dirs::home_dir().context("home")?)
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", "/tmp");
    let (returncode, reason) = match open.output() {
        Ok(output) if !bounded_output(&output.stdout) || !bounded_output(&output.stderr) => {
            (None, "open_output_exceeded_bound")
        }
        Ok(output) if output.status.success() => {
            let _ = Command::new("/usr/bin/osascript")
                .args(["-e", WATCHDOG_WINDOW_SCRIPT])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear()
                .env("HOME", dirs::home_dir().context("home")?)
                .env("PATH", "/usr/bin:/bin")
                .env("TMPDIR", "/tmp")
                .status();
            (Some(0), "")
        }
        Ok(_) => (Some(1), "open_failed"),
        Err(_) => (None, "OSError"),
    };
    let state = if returncode == Some(0) {
        "launch_requested"
    } else {
        "open_failed"
    };
    write_status(
        &status_path,
        &status_payload(state, reason, &launches, next_attempt, timestamp, &digest),
    )?;
    Ok(if returncode == Some(0) { 0 } else { 1 })
}

pub fn cmd_auto_reply_host(opts: AutoReplyHostOptions) -> Result<()> {
    if opts.action == AutoReplyHostAction::Tick {
        let manifest = opts
            .manifest
            .as_deref()
            .context("auto-reply-host --tick requires --manifest")?;
        let state_root = opts
            .state_root
            .as_deref()
            .context("auto-reply-host --tick requires --state-root")?;
        let code = run_tick(manifest, state_root)?;
        if opts.json {
            crate::util::output_json(&json!({
                "command": "auto-reply-host",
                "action": "tick",
                "owns_ax": false,
                "service_kind": "terminal_monitor",
                "exit_code": code,
            }))?;
        }
        if code != 0 {
            bail!("session monitor tick failed");
        }
        return Ok(());
    }

    let config = config::load_config()?;
    match opts.action {
        AutoReplyHostAction::Tick => unreachable!("tick handled above"),
        AutoReplyHostAction::Status => {
            let script = script_path("status-auto-reply-service.sh")?;
            let (code, stdout, stderr) = run_script(&script, &[])?;
            let healthy = stdout.lines().any(|line| line == "healthy=true");
            let kind = stdout
                .lines()
                .find_map(|line| line.strip_prefix("service_kind="))
                .unwrap_or("");
            emit(
                opts.json,
                json!({
                    "command": "auto-reply-host",
                    "action": "status",
                    "service_kind": kind,
                    "healthy": healthy,
                    "label": MONITOR_LABEL,
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": code,
                }),
                &stdout,
                &stderr,
                code,
            )
        }
        AutoReplyHostAction::Disable => {
            let script = script_path("uninstall-auto-reply-session-monitor.sh")?;
            let (code, stdout, stderr) = run_script(&script, &[])?;
            emit(
                opts.json,
                json!({
                    "command": "auto-reply-host",
                    "action": "disable",
                    "label": MONITOR_LABEL,
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": code,
                }),
                &stdout,
                &stderr,
                code,
            )
        }
        AutoReplyHostAction::Bake => {
            let python = configured_python(&config)?;
            let chats = configured_chats(&config, &opts.chats)?;
            let packager = script_path("prepare-auto-reply-session-runtime.py")?;
            let bin = std::env::current_exe().context("resolve openkakao-cli path")?;
            let config_path = config::config_path()?;
            let mut args = vec![
                packager.to_string_lossy().into_owned(),
                "--bin".to_string(),
                bin.to_string_lossy().into_owned(),
                "--python".to_string(),
                python.to_string_lossy().into_owned(),
                "--config".to_string(),
                config_path.to_string_lossy().into_owned(),
            ];
            for chat in &chats {
                args.push("--chat".to_string());
                args.push(chat.clone());
            }
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let (code, stdout, stderr) = run_script(&python, &arg_refs)?;
            let staged: serde_json::Value =
                serde_json::from_str(stdout.trim()).unwrap_or_else(|_| json!({ "raw": stdout }));
            emit(
                opts.json,
                json!({
                    "command": "auto-reply-host",
                    "action": "bake",
                    "activated": false,
                    "owns_ax": false,
                    "service_kind": "terminal_monitor",
                    "label": MONITOR_LABEL,
                    "runtime": staged,
                    "stderr": stderr,
                    "exit_code": code,
                }),
                &stdout,
                &stderr,
                code,
            )
        }
    }
}
