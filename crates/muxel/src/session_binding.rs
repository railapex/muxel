//! Exact provider session bindings that must survive an in-process `/resume`.

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash as _, Hasher as _};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use uuid::Uuid;

const CLAUDE_BINDING_DIR: &str = "provider-session-bindings/claude";
const CLAUDE_SETTINGS_DIR: &str = "provider-session-bindings/claude/settings";
const CLAUDE_HOOK_FLAG: &str = "--claude-session-hook";
const MAX_HOOK_INPUT_BYTES: u64 = 64 * 1024;
/// The field the hook adds to Claude's event: see [`ClaudeHookStamp`].
const HOOK_STAMP_FIELD: &str = "muxel_hook";
pub(crate) const MUXEL_INSTANCE_ID_ENV: &str = "MUXEL_INSTANCE_ID";

/// What the hook adds to the record it writes: a nonce naming this write, and
/// when it ran. A record with a nonce muxel hasn't seen means a SessionStart ran
/// since the last one, even when it names the conversation already bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeHookStamp {
    pub nonce: Uuid,
    pub recorded_at: SystemTime,
}

/// A pane's binding record, validated, from one read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeBinding {
    pub session_id: String,
    /// `None` for a record written by an older muxel's hook.
    pub stamp: Option<ClaudeHookStamp>,
}

/// Whether a pane's binding record reports a switch, given the nonce of the
/// last record this launch read (`seen`) and the bound conversation. A stamped
/// record is known by its nonce: one this launch hasn't seen is a switch even
/// when it names the bound conversation, as after `/resume B` and `/resume A`
/// between two reads. The launch's first stamped record, and a record from an
/// older muxel's hook, switch only when they name another conversation: a
/// resumed launch's own `SessionStart` names the conversation muxel launched.
pub(crate) fn claude_record_switches(
    binding: &ClaudeBinding,
    seen: Option<Uuid>,
    bound: Option<&str>,
) -> bool {
    match (binding.stamp, seen) {
        (Some(stamp), Some(seen)) => stamp.nonce != seen,
        _ => bound != Some(binding.session_id.as_str()),
    }
}

pub(crate) fn hook_instance_from_args(
    args: impl IntoIterator<Item = OsString>,
    env_instance_id: Option<OsString>,
) -> std::result::Result<Option<Uuid>, String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == CLAUDE_HOOK_FLAG {
            // Old settings files passed the UUID as argv. Keep accepting that form so
            // hooks from an already-running pane survive a Muxel upgrade. New settings
            // route the pane through inherited environment instead.
            let raw = args.next().or(env_instance_id).ok_or_else(|| {
                format!(
                    "{CLAUDE_HOOK_FLAG} requires an instance UUID argument or {MUXEL_INSTANCE_ID_ENV}"
                )
            })?;
            let raw = raw
                .to_str()
                .ok_or_else(|| format!("{CLAUDE_HOOK_FLAG} requires a UTF-8 instance UUID"))?;
            let id = Uuid::parse_str(raw)
                .map_err(|_| format!("{CLAUDE_HOOK_FLAG} requires a valid instance UUID"))?;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub(crate) fn run_claude_session_hook(instance_id: Uuid) -> Result<()> {
    let data_dir = muxel_store::data_dir().context("could not determine muxel data directory")?;
    write_claude_binding_from_reader(&data_dir, instance_id, std::io::stdin().lock())
}

pub(crate) fn claude_hook_settings() -> Option<String> {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            log::warn!("could not resolve Muxel executable for Claude session hook: {error}");
            return None;
        }
    };
    let Some(data_dir) = muxel_store::data_dir() else {
        log::warn!("could not resolve Muxel data directory for Claude session hook");
        return None;
    };
    let path = match write_claude_hook_settings_file(&data_dir, &executable) {
        Ok(path) => path,
        Err(error) => {
            log::warn!("could not write Claude session hook settings: {error:#}");
            return None;
        }
    };
    let argument = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    if !windows_batch_arg_is_safe(&argument) {
        log::warn!(
            "Claude hook settings path contains cmd.exe metacharacters; session rebinding is disabled for this pane"
        );
        return None;
    }
    Some(argument)
}

fn claude_hook_settings_for(executable: &Path) -> String {
    json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "resume|clear|fork",
                "hooks": [{
                    "type": "command",
                    "command": executable.to_string_lossy(),
                    "args": [CLAUDE_HOOK_FLAG],
                    "timeout": 5
                }]
            }]
        }
    })
    .to_string()
}

fn executable_identity(executable: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    executable.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn claude_hook_settings_path(data_dir: &Path, executable: &Path) -> PathBuf {
    data_dir
        .join(CLAUDE_SETTINGS_DIR)
        .join(format!("{}.json", executable_identity(executable)))
}

fn write_claude_hook_settings_file(data_dir: &Path, executable: &Path) -> Result<PathBuf> {
    let path = claude_hook_settings_path(data_dir, executable);
    let parent = path
        .parent()
        .context("Claude hook settings path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "creating Claude hook settings directory {}",
            parent.display()
        )
    })?;
    let desired = claude_hook_settings_for(executable);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
        return Ok(path);
    }

    let temp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    std::fs::write(&temp, &desired)
        .with_context(|| format!("writing Claude hook settings {}", temp.display()))?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        // Two pane launches can race. Identical content at the target is success;
        // only remove the uniquely-owned temporary file.
        if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
            let _ = std::fs::remove_file(&temp);
        } else {
            let _ = std::fs::remove_file(&temp);
            return Err(error)
                .with_context(|| format!("installing Claude hook settings {}", path.display()));
        }
    }
    Ok(path)
}

#[cfg(any(windows, test))]
fn windows_batch_arg_is_safe(value: &str) -> bool {
    !value
        .chars()
        .any(|ch| matches!(ch, '"' | '%' | '&' | '|' | '<' | '>' | '^' | '\r' | '\n'))
}

pub(crate) fn claude_binding_path(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(CLAUDE_BINDING_DIR)
        .join(format!("{instance_id}.json"))
}

fn write_claude_binding_from_reader(
    data_dir: &Path,
    instance_id: Uuid,
    reader: impl std::io::Read,
) -> Result<()> {
    let mut input = String::new();
    reader
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_string(&mut input)
        .context("reading Claude SessionStart input")?;
    if input.len() as u64 > MAX_HOOK_INPUT_BYTES {
        bail!("Claude SessionStart input exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    let mut event: Value =
        serde_json::from_str(&input).context("parsing Claude SessionStart input")?;
    validate_claude_event(&event)?;
    stamp_claude_event(&mut event, Uuid::new_v4(), SystemTime::now());

    let path = claude_binding_path(data_dir, instance_id);
    let parent = path.parent().context("Claude binding path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating Claude binding directory {}", parent.display()))?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, event.to_string())
        .with_context(|| format!("writing Claude binding {}", temp.display()))?;
    // muxel may claim the previous record (rename it away) at any moment.
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error)
                .with_context(|| format!("replacing Claude binding {}", path.display()));
        }
        _ => {}
    }
    std::fs::rename(&temp, &path)
        .with_context(|| format!("installing Claude binding {}", path.display()))?;
    Ok(())
}

fn validate_claude_event(event: &Value) -> Result<()> {
    if event.get("hook_event_name").and_then(Value::as_str) != Some("SessionStart") {
        bail!("expected a Claude SessionStart event");
    }
    let source = event
        .get("source")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no source")?;
    if !matches!(source, "resume" | "clear" | "fork") {
        bail!("unexpected Claude SessionStart source {source:?}");
    }
    let session_id = event
        .get("session_id")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no session_id")?;
    Uuid::parse_str(session_id).context("Claude SessionStart session_id is not a UUID")?;
    let transcript = event
        .get("transcript_path")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no transcript_path")?;
    let expected_name = format!("{session_id}.jsonl");
    if Path::new(transcript)
        .file_name()
        .and_then(|name| name.to_str())
        != Some(expected_name.as_str())
    {
        bail!("Claude transcript filename does not match session_id");
    }
    let cwd = event
        .get("cwd")
        .and_then(Value::as_str)
        .context("Claude SessionStart event has no cwd")?;
    if !Path::new(cwd).is_absolute() || !Path::new(transcript).is_absolute() {
        bail!("Claude SessionStart paths must be absolute");
    }
    Ok(())
}

fn stamp_claude_event(event: &mut Value, nonce: Uuid, recorded_at: SystemTime) {
    let micros = recorded_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros() as u64);
    if let Some(event) = event.as_object_mut() {
        event.insert(
            HOOK_STAMP_FIELD.to_string(),
            json!({ "nonce": nonce.to_string(), "recorded_at_us": micros }),
        );
    }
}

fn claude_hook_stamp(event: &Value) -> Option<ClaudeHookStamp> {
    let stamp = event.get(HOOK_STAMP_FIELD)?;
    let nonce = Uuid::parse_str(stamp.get("nonce")?.as_str()?).ok()?;
    let micros = stamp.get("recorded_at_us")?.as_u64()?;
    Some(ClaudeHookStamp {
        nonce,
        recorded_at: SystemTime::UNIX_EPOCH + Duration::from_micros(micros),
    })
}

/// The pane's binding record, if one is present and valid.
#[cfg(test)]
fn read_claude_binding(data_dir: &Path, instance_id: Uuid, home: &Path) -> Option<ClaudeBinding> {
    claude_binding_from_record(
        &std::fs::read(claude_binding_path(data_dir, instance_id)).ok()?,
        home,
    )
}

/// Parse and validate one read of a binding record. The session id, its
/// transcript and the hook's stamp all come from the same bytes.
pub(crate) fn claude_binding_from_record(record: &[u8], home: &Path) -> Option<ClaudeBinding> {
    let event: Value = serde_json::from_slice(record).ok()?;
    validate_claude_event(&event).ok()?;
    let session_id = event.get("session_id")?.as_str()?;
    let source = event.get("source")?.as_str()?;
    let transcript = Path::new(event.get("transcript_path")?.as_str()?);
    let projects = home.join(".claude").join("projects").canonicalize().ok()?;
    let expected_name = format!("{session_id}.jsonl");
    if transcript.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return None;
    }

    // `/clear` and `/fork` may announce the new UUID before Claude writes its
    // first transcript record. Validate the existing project directory in that
    // case; resumed sessions must already have a real transcript.
    let project = transcript.parent()?.canonicalize().ok()?;
    if project.parent()? != projects {
        return None;
    }
    match transcript.canonicalize() {
        Ok(resolved) if resolved.is_file() && resolved.parent() == Some(project.as_path()) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(source, "clear" | "fork") => {}
        _ => return None,
    }
    Some(ClaudeBinding {
        session_id: session_id.to_string(),
        stamp: claude_hook_stamp(&event),
    })
}

/// A binding record muxel has claimed: its bytes, read once, and its write
/// time (an older muxel's hook didn't stamp its record).
pub(crate) struct ClaimedClaudeRecord {
    path: PathBuf,
    pub bytes: Vec<u8>,
    pub modified: Option<SystemTime>,
}

/// Where a pane's binding record moves while muxel handles it. The hook never
/// writes this name, so a record it writes meanwhile lands at the shared path.
fn claude_claimed_binding_path(data_dir: &Path, instance_id: Uuid) -> PathBuf {
    data_dir
        .join(CLAUDE_BINDING_DIR)
        .join(format!("{instance_id}.claimed.json"))
}

/// Claim the pane's next binding record: one claimed before whose handling
/// failed, else the shared record, moved to the claimed name by an atomic
/// rename. Renaming keeps its write time.
pub(crate) fn claim_claude_binding(
    data_dir: &Path,
    instance_id: Uuid,
) -> Option<ClaimedClaudeRecord> {
    let path = claude_claimed_binding_path(data_dir, instance_id);
    if !path.exists() {
        std::fs::rename(claude_binding_path(data_dir, instance_id), &path).ok()?;
    }
    let bytes = std::fs::read(&path).ok()?;
    let modified = std::fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok();
    Some(ClaimedClaudeRecord {
        path,
        bytes,
        modified,
    })
}

/// Delete a claimed record once it has been handled. Only the claimed file
/// goes; a record the hook has written since waits for the next claim.
pub(crate) fn acknowledge_claude_binding(record: ClaimedClaudeRecord) {
    let _ = std::fs::remove_file(record.path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-claude-binding-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run the hook as Claude does on `/resume <session_id>` in `cwd`.
    fn resume_hook(data: &Path, home: &Path, cwd: &Path, instance_id: Uuid, session_id: &str) {
        let transcript = muxel_core::claude_session_path(home, cwd, session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
    }

    fn claimed_session(record: &ClaimedClaudeRecord, home: &Path) -> String {
        claude_binding_from_record(&record.bytes, home)
            .unwrap()
            .session_id
    }

    #[test]
    fn hook_settings_use_exec_form_and_resume_sources() {
        let settings = claude_hook_settings_for(Path::new("C:/Program Files/muxel.exe"));
        let value: Value = serde_json::from_str(&settings).unwrap();
        let handler = &value["hooks"]["SessionStart"][0];
        assert_eq!(handler["matcher"], "resume|clear|fork");
        assert_eq!(handler["hooks"][0]["command"], "C:/Program Files/muxel.exe");
        assert_eq!(handler["hooks"][0]["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(handler["hooks"][0]["args"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hook_settings_are_passed_by_batch_safe_file() {
        let root = temp_dir();
        let executable = Path::new("C:/Program Files/muxel.exe");
        let path = write_claude_hook_settings_file(&root, executable).unwrap();
        let same_path = write_claude_hook_settings_file(&root, executable).unwrap();
        let argument = path.to_string_lossy();
        assert!(windows_batch_arg_is_safe(&argument));
        assert!(!argument.contains("SessionStart"));
        assert_eq!(path, same_path);
        assert_ne!(
            path,
            claude_hook_settings_path(&root, Path::new("C:/Other/muxel.exe"))
        );

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let hook = &value["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(hook["command"], "C:/Program Files/muxel.exe");
        assert_eq!(hook["args"][0], CLAUDE_HOOK_FLAG);
        assert_eq!(hook["args"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn valid_resume_event_accepts_a_transcript_from_another_claude_project() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript_cwd = root.join("moved-worktree");
        let transcript = muxel_core::claude_session_path(&home, &transcript_cwd, &session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();

        assert_eq!(
            read_claude_binding(&data, instance_id, &home)
                .map(|binding| binding.session_id)
                .as_deref(),
            Some(session_id.as_str())
        );

        let outside = root.join(format!("{session_id}.jsonl"));
        std::fs::write(&outside, "{}\n").unwrap();
        let outside_event = json!({
            "session_id": session_id,
            "transcript_path": outside,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(outside_event.to_string()),
        )
        .unwrap();
        assert_eq!(
            read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
            None
        );

        write_claude_binding_from_reader(
            &data,
            instance_id,
            std::io::Cursor::new(event.to_string()),
        )
        .unwrap();
        std::fs::remove_file(&transcript).unwrap();
        assert_eq!(
            read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
            None
        );

        for source in ["clear", "fork"] {
            let pending_event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": source
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(pending_event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home)
                    .map(|binding| binding.session_id)
                    .as_deref(),
                Some(session_id.as_str()),
                "{source} should bind before Claude writes the first transcript record"
            );
        }
        let record = claim_claude_binding(&data, instance_id).unwrap();
        acknowledge_claude_binding(record);
        assert!(!claude_binding_path(&data, instance_id).exists());
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_to_b_to_a_between_polls_reads_as_a_switch() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();
        let resume = |session_id: &str| resume_hook(&data, &home, &cwd, instance_id, session_id);

        // `claude --resume A` runs the hook as it launches, naming A.
        let before = SystemTime::now();
        resume(&a);
        let first = read_claude_binding(&data, instance_id, &home).unwrap();
        let stamp = first.stamp.expect("the hook stamps its record");
        assert!(before <= stamp.recorded_at && stamp.recorded_at <= SystemTime::now());
        assert!(!claude_record_switches(&first, None, Some(a.as_str())));

        // `/resume B` and then `/resume A`, both before muxel's next read.
        resume(&b);
        resume(&a);
        let last = read_claude_binding(&data, instance_id, &home).unwrap();
        assert_eq!(last.session_id, a);
        assert!(
            claude_record_switches(&last, Some(stamp.nonce), Some(a.as_str())),
            "a record muxel hasn't seen is a switch, whatever it names"
        );
        // The same record read again, because removing it failed, is not.
        let nonce = last.stamp.map(|stamp| stamp.nonce);
        assert!(!claude_record_switches(&last, nonce, Some(a.as_str())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_older_hooks_record_switches_only_to_another_conversation() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let transcript = muxel_core::claude_session_path(&home, &cwd, &session_id);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        // An older muxel's hook wrote Claude's event as it came.
        let path = claude_binding_path(&data, instance_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let event = json!({
            "session_id": session_id,
            "transcript_path": transcript,
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "source": "resume"
        });
        std::fs::write(&path, event.to_string()).unwrap();
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();

        let record = claim_claude_binding(&data, instance_id).unwrap();
        // Its write time stands in for the stamp; claiming it keeps that.
        assert_eq!(record.modified, Some(written));
        let binding = claude_binding_from_record(&record.bytes, &home).unwrap();
        assert_eq!(binding.stamp, None);
        let seen = Some(Uuid::new_v4());
        assert!(!claude_record_switches(
            &binding,
            seen,
            Some(session_id.as_str())
        ));
        let other = Uuid::new_v4().to_string();
        assert!(claude_record_switches(&binding, seen, Some(other.as_str())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_written_while_muxel_handles_the_last_survives_its_acknowledgement() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        resume_hook(&data, &home, &cwd, instance_id, &a);
        let claimed = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&claimed, &home), a);
        // The hook runs again between muxel's read and its acknowledgement.
        resume_hook(&data, &home, &cwd, instance_id, &b);
        acknowledge_claude_binding(claimed);

        let next = claim_claude_binding(&data, instance_id).expect("B wasn't deleted unread");
        assert_eq!(claimed_session(&next, &home), b);
        acknowledge_claude_binding(next);
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_whose_handling_failed_is_claimed_again_before_a_newer_one() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let cwd = root.join("project");
        let instance_id = Uuid::new_v4();
        let a = Uuid::new_v4().to_string();
        let b = Uuid::new_v4().to_string();

        resume_hook(&data, &home, &cwd, instance_id, &a);
        let claimed = claim_claude_binding(&data, instance_id).unwrap();
        // Saving the binding failed, so the record isn't acknowledged; the
        // hook runs again before the next tick.
        drop(claimed);
        resume_hook(&data, &home, &cwd, instance_id, &b);

        let retry = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&retry, &home), a);
        acknowledge_claude_binding(retry);
        let next = claim_claude_binding(&data, instance_id).unwrap();
        assert_eq!(claimed_session(&next, &home), b);
        acknowledge_claude_binding(next);
        assert!(claim_claude_binding(&data, instance_id).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_non_resume_events_and_mismatched_transcript_names() {
        let root = temp_dir();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        for event in [
            json!({
                "session_id": session_id,
                "transcript_path": root.join(format!("{session_id}.jsonl")),
                "cwd": root,
                "hook_event_name": "SessionStart",
                "source": "startup"
            }),
            json!({
                "session_id": session_id,
                "transcript_path": root.join("other.jsonl"),
                "cwd": root,
                "hook_event_name": "SessionStart",
                "source": "resume"
            }),
        ] {
            assert!(
                write_claude_binding_from_reader(
                    &root,
                    instance_id,
                    std::io::Cursor::new(event.to_string()),
                )
                .is_err()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_transcript_paths_that_escape_or_nest_below_the_project_store() {
        let root = temp_dir();
        let data = root.join("data");
        let home = root.join("home");
        let projects = home.join(".claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4().to_string();
        let cwd = root.join("project");

        let outside = home.join(".claude").join(format!("{session_id}.jsonl"));
        std::fs::write(&outside, "{}\n").unwrap();
        let traversal = projects.join("..").join(format!("{session_id}.jsonl"));

        let nested = projects
            .join("project")
            .join("nested")
            .join(format!("{session_id}.jsonl"));
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, "{}\n").unwrap();

        for transcript in [traversal, nested] {
            let event = json!({
                "session_id": session_id,
                "transcript_path": transcript,
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "resume"
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
                None
            );
        }

        #[cfg(unix)]
        {
            let escaped = root.join("escaped-project");
            std::fs::create_dir_all(&escaped).unwrap();
            std::fs::write(escaped.join(format!("{session_id}.jsonl")), "{}\n").unwrap();
            let linked = projects.join("linked-project");
            std::os::unix::fs::symlink(&escaped, &linked).unwrap();
            let event = json!({
                "session_id": session_id,
                "transcript_path": linked.join(format!("{session_id}.jsonl")),
                "cwd": cwd,
                "hook_event_name": "SessionStart",
                "source": "resume"
            });
            write_claude_binding_from_reader(
                &data,
                instance_id,
                std::io::Cursor::new(event.to_string()),
            )
            .unwrap();
            assert_eq!(
                read_claude_binding(&data, instance_id, &home).map(|binding| binding.session_id),
                None
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hook_mode_requires_one_valid_instance_uuid() {
        let id = Uuid::new_v4();
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG), id.to_string().into()],
                None
            ),
            Ok(Some(id))
        );
        assert_eq!(
            hook_instance_from_args(
                [OsString::from(CLAUDE_HOOK_FLAG)],
                Some(id.to_string().into())
            ),
            Ok(Some(id))
        );
        assert!(hook_instance_from_args([OsString::from(CLAUDE_HOOK_FLAG)], None).is_err());
        assert_eq!(
            hook_instance_from_args([OsString::from("--other")], Some(id.to_string().into())),
            Ok(None)
        );
    }
}
