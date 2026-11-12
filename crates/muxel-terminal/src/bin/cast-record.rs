//! Records a provider session as a cast (see
//! `crates/muxel-terminal/src/fixtures/casts/README.md`) through muxel's own
//! PTY session, so the provider runs in the environment a muxel pane gives it
//! and gets muxel's answers to its terminal queries.
//!
//! ```text
//! cast-record --steps <steps.json> --out <cast.jsonl> [--hook-log <log>] -- <program> [args...]
//! ```
//!
//! The step file is `{"provider", "version", "launch"?, "pty"?, "cols"?,
//! "rows"?, "steps": [...]}`. `launch` is the arguments as the cast should
//! show them, with no paths. `pty` names the console when the recorder can't
//! (a side-by-side `conpty.dll`'s version). Each step has a `kind`:
//!
//! | kind | fields | does |
//! |---|---|---|
//! | `quiet` | `a`, `b` | waits until no output has come for `a` ms since the step began; gives up after `b` ms |
//! | `waitfor` | `text`, `b` | waits until `text` appears in output read since the step began; gives up after `b` ms |
//! | `sleep` | `a` | waits `a` ms |
//! | `type` | `text`, `a` | writes `text` one character at a time, `a` ms apart |
//! | `paste` | `text` | writes `text` as one bracketed paste |
//! | `bytes` | `text` | writes the bytes `text` spells in hex |
//! | `focus` | `value` | gives the pane focus (`true`) or takes it away |
//! | `mark` | `mark` | records a mark: `{"id", "by", "truth", "note"?}` |
//! | `waitexit` | `b` | waits up to `b` ms for the program to exit |
//!
//! A step that gives up, or the program exiting before a `waitexit`, stops the
//! recording with an error, so no mark is written for a state the session
//! never reached. The cast is written either way, holding what was recorded.
//!
//! `--hook-log` names a file the provider's own hooks append to, one JSON
//! object per line with `t_unix_ms` and `name`; each becomes a `hook` event.
//!
//! The cast is not sanitized (`"sanitized": false`) and carries no
//! `out_sha256`: sanitize it, which also hashes it, before it goes anywhere.

use muxel_terminal::{CommandSpec, PtyChunk, TerminalSession};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(20);

fn usage() -> ! {
    eprintln!(
        "usage: cast-record --steps <steps.json> --out <cast.jsonl> [--hook-log <log>] \
         -- <program> [args...]"
    );
    std::process::exit(2);
}

/// What the recording holds so far, shared with the output thread.
struct Recording {
    started: Instant,
    events: Vec<(u64, &'static str, Value)>,
    output: Vec<u8>,
    last_output: Instant,
    exited: bool,
    /// Set before the recorder kills the program, whose exit is then not the
    /// program's own and isn't recorded.
    killing: bool,
}

impl Recording {
    fn push(&mut self, kind: &'static str, payload: Value) {
        let t = self.started.elapsed().as_millis() as u64;
        self.events.push((t, kind, payload));
    }
}

type Shared = Arc<(Mutex<Recording>, Condvar)>;

/// Bytes as the cast format stores them: a string when they are valid UTF-8
/// on their own, else `{"hex": ...}`.
fn bytes_payload(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::String(text.to_string()),
        Err(_) => json!({ "hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>() }),
    }
}

fn hex_bytes(text: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(text.len().is_multiple_of(2), "odd-length hex {text:?}");
    (0..text.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(&text[i..i + 2], 16)?))
        .collect()
}

/// Waits until `done` holds, the program exits, or `limit` passes.
fn wait_until(
    shared: &Shared,
    limit: Duration,
    mut done: impl FnMut(&Recording) -> bool,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + limit;
    let (lock, signal) = &**shared;
    let mut recording = lock.lock().expect("recording lock");
    loop {
        if done(&recording) {
            return Ok(());
        }
        anyhow::ensure!(!recording.exited, "the program exited");
        let now = Instant::now();
        anyhow::ensure!(now < deadline, "gave up after {} ms", limit.as_millis());
        recording = signal
            .wait_timeout(recording, POLL.min(deadline - now))
            .expect("recording lock")
            .0;
    }
}

fn write_input(shared: &Shared, session: &TerminalSession, bytes: &[u8]) -> anyhow::Result<()> {
    let (lock, _) = &**shared;
    let mut recording = lock.lock().expect("recording lock");
    anyhow::ensure!(!recording.exited, "the program exited");
    session.write_input(bytes);
    recording.push("in", bytes_payload(bytes));
    Ok(())
}

fn run_step(shared: &Shared, session: &TerminalSession, step: &Value) -> anyhow::Result<()> {
    let ms = |field: &str| Duration::from_millis(step[field].as_u64().unwrap_or(0));
    let text = step["text"].as_str().unwrap_or_default();
    match step["kind"].as_str().unwrap_or_default() {
        "quiet" => {
            // Counted from the step's start too: after a key that draws
            // nothing for a while, quiet from before the key isn't quiet
            // after it.
            let quiet = ms("a");
            let begun = Instant::now();
            wait_until(shared, ms("b"), |r| {
                r.last_output.max(begun).elapsed() >= quiet
            })
        }
        "waitfor" => {
            let from = shared.0.lock().expect("recording lock").output.len();
            let needle = text.as_bytes();
            wait_until(shared, ms("b"), |r| {
                r.output[from..].windows(needle.len()).any(|w| w == needle)
            })
        }
        "sleep" => {
            std::thread::sleep(ms("a"));
            Ok(())
        }
        "type" => {
            for ch in text.chars() {
                write_input(shared, session, ch.to_string().as_bytes())?;
                std::thread::sleep(ms("a"));
            }
            Ok(())
        }
        "paste" => write_input(
            shared,
            session,
            format!("\x1b[200~{text}\x1b[201~").as_bytes(),
        ),
        "bytes" => write_input(shared, session, &hex_bytes(text)?),
        "focus" => {
            let focused = step["value"].as_bool().unwrap_or(true);
            let (lock, _) = &**shared;
            let mut recording = lock.lock().expect("recording lock");
            session.report_focus(focused);
            recording.push("focus", Value::Bool(focused));
            Ok(())
        }
        "mark" => {
            let mark = &step["mark"];
            anyhow::ensure!(mark["id"].is_string(), "a mark needs an id");
            shared
                .0
                .lock()
                .expect("recording lock")
                .push("mark", mark.clone());
            Ok(())
        }
        "waitexit" => wait_until(shared, ms("b"), |r| r.exited),
        other => anyhow::bail!("unknown step kind {other:?}"),
    }
}

/// The provider's hook events, timed against the recording's start.
fn hook_events(path: &PathBuf, started_unix_ms: u64) -> anyhow::Result<Vec<(u64, Value)>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut record: Map<String, Value> = serde_json::from_str(line)?;
        let at = record
            .remove("t_unix_ms")
            .and_then(|at| at.as_u64())
            .ok_or_else(|| anyhow::anyhow!("hook line without t_unix_ms: {line}"))?;
        anyhow::ensure!(
            record.contains_key("name"),
            "hook line without name: {line}"
        );
        events.push((at.saturating_sub(started_unix_ms), Value::Object(record)));
    }
    Ok(events)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let (mut steps_path, mut out_path, mut hook_log) = (None, None, None);
    loop {
        let flag = args.next().unwrap_or_else(|| usage());
        let mut value = || args.next().map(PathBuf::from).unwrap_or_else(|| usage());
        match flag.to_str() {
            Some("--steps") => steps_path = Some(value()),
            Some("--out") => out_path = Some(value()),
            Some("--hook-log") => hook_log = Some(value()),
            Some("--") => break,
            _ => usage(),
        }
    }
    let (Some(steps_path), Some(out_path)) = (steps_path, out_path) else {
        usage();
    };
    let program = args.next().unwrap_or_else(|| usage());
    let program = program.to_string_lossy().into_owned();
    let child_args: Vec<String> = args.map(|arg| arg.to_string_lossy().into_owned()).collect();

    let plan: Value = serde_json::from_str(&std::fs::read_to_string(&steps_path)?)?;
    let provider = plan["provider"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the step file needs a provider"))?;
    let version = plan["version"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("the step file needs the provider's version"))?;
    let cols = plan["cols"].as_u64().unwrap_or(120) as u16;
    let rows = plan["rows"].as_u64().unwrap_or(40) as u16;
    let steps = plan["steps"].as_array().cloned().unwrap_or_default();

    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let spec = CommandSpec::program(program, child_args).with_cwd(cwd);
    let started_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let started = Instant::now();
    let shared: Shared = Arc::new((
        Mutex::new(Recording {
            started,
            events: Vec::new(),
            output: Vec::new(),
            last_output: started,
            exited: false,
            killing: false,
        }),
        Condvar::new(),
    ));
    let (session, rx) = TerminalSession::spawn(spec, cols, rows)?;

    let output = std::thread::Builder::new()
        .name("cast-record-output".to_string())
        .spawn({
            let shared = shared.clone();
            let session = session.clone();
            move || {
                let (lock, signal) = &*shared;
                while let Ok(chunk) = rx.recv_blocking() {
                    let mut recording = lock.lock().expect("recording lock");
                    match chunk {
                        PtyChunk::Output(bytes) => {
                            recording.push("out", bytes_payload(&bytes));
                            recording.output.extend_from_slice(&bytes);
                            recording.last_output = Instant::now();
                            // Parse at once: some queries (the cursor position)
                            // are answered by the parser, as in a pane.
                            session.process_probe_output(&bytes);
                        }
                        PtyChunk::Exit { code, .. } => {
                            if !recording.killing {
                                recording.push("exit", json!({ "code": code, "eof": true }));
                            }
                            recording.exited = true;
                            signal.notify_all();
                            break;
                        }
                    }
                    signal.notify_all();
                }
            }
        })?;

    let mut failure = None;
    for (index, step) in steps.iter().enumerate() {
        if let Err(error) = run_step(&shared, &session, step) {
            failure = Some(format!("step {index} ({step}): {error}"));
            break;
        }
    }

    let still_running = {
        let mut recording = shared.0.lock().expect("recording lock");
        recording.killing = !recording.exited;
        recording.killing
    };
    if still_running {
        session.kill();
    }
    drop(session);
    let _ = output.join();

    let recording = shared.0.lock().expect("recording lock");
    let mut events: Vec<(u64, &str, Value)> = recording.events.clone();
    if let Some(log) = &hook_log {
        for (t, payload) in hook_events(log, started_unix_ms)? {
            events.push((t, "hook", payload));
        }
        events.sort_by_key(|(t, _, _)| *t);
    }
    let mut header = json!({
        "muxel_cast": 1,
        "provider": provider,
        "version": version,
        "os": std::env::consts::OS,
        "pty": plan.get("pty").cloned().unwrap_or_else(|| json!(default_pty())),
        "tap": "muxel",
        "cols": cols,
        "rows": rows,
        "term": "xterm-256color",
        "replies": { "osc7501": true },
        "launch": plan.get("launch").cloned().unwrap_or_else(|| json!([])),
        "recorded": recorded_date(started_unix_ms),
        "sanitized": false,
    });
    // The program inherits this environment; a provider that honours
    // NO_COLOR draws no colour, which changes what the readers see.
    if std::env::var_os("NO_COLOR").is_some() {
        header["no_color"] = json!(true);
    }
    let mut cast = serde_json::to_string(&header)? + "\n";
    for (t, kind, payload) in events {
        cast += &serde_json::to_string(&json!([t, kind, payload]))?;
        cast.push('\n');
    }
    std::fs::write(&out_path, cast)?;
    match failure {
        Some(failure) => anyhow::bail!("recording stopped at {failure}"),
        None => Ok(()),
    }
}

/// The header's `pty` when the step file doesn't name one. On Windows the
/// pty layer loads a `conpty.dll` found beside the executable in place of the
/// system console, and the two answer a provider differently.
fn default_pty() -> &'static str {
    if !cfg!(windows) {
        return "unix pty";
    }
    let side_by_side = std::env::current_exe()
        .map(|exe| exe.with_file_name("conpty.dll").is_file())
        .unwrap_or(false);
    if side_by_side {
        "conpty (conpty.dll beside the recorder)"
    } else {
        "conpty (system)"
    }
}

/// `YYYY-MM-DD` in UTC for a Unix time in milliseconds.
fn recorded_date(unix_ms: u64) -> String {
    // Days to a civil date (Howard Hinnant's algorithm).
    let days = (unix_ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::{bytes_payload, hex_bytes, recorded_date};
    use serde_json::json;

    #[test]
    fn a_read_ending_inside_a_character_is_kept_as_hex() {
        assert_eq!(bytes_payload(b"ok"), json!("ok"));
        assert_eq!(bytes_payload(&[0xe2, 0x94]), json!({ "hex": "e294" }));
        assert_eq!(hex_bytes("0d1b").unwrap(), b"\r\x1b");
    }

    #[test]
    fn recorded_date_is_the_utc_day() {
        assert_eq!(recorded_date(0), "1970-01-01");
        assert_eq!(recorded_date(951_782_400_000), "2000-02-29");
        assert_eq!(recorded_date(1_794_355_200_000), "2026-11-11");
    }
}
