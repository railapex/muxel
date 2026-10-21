//! Program Status Protocol (OSC 7501), revision 0.4.
//!
//! The reader frames and validates reports before the UI sees the PTY bytes.
//! Only the root record drives a pane badge; child ids are valid protocol input
//! but describe work inside the pane, not the pane's own lifecycle.

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

pub(crate) const CAPABILITY_REPLY: &str =
    "\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";
const MAX_SEQUENCE: usize = 4096;
const TEXT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub state: ProgramState,
    pub id: Option<String>,
    pub kind: Option<ProgramStatusKind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Query,
    Report(Report),
    Reset,
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.+-".contains(&byte))
}

fn valid_id(id: &str) -> bool {
    let mut count = 0;
    for segment in id.split('/') {
        count += 1;
        if count > 8 || segment.len() > 32 || !valid_token(segment) {
            return false;
        }
    }
    count > 0
}

fn decode_text(encoded: &str, decoded_limit: usize) -> Option<String> {
    let bytes = TEXT_BASE64.decode(encoded).ok()?;
    if bytes.len() > decoded_limit {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text
        .chars()
        .any(|ch| matches!(ch as u32, 0..=31 | 127..=159))
    {
        return None;
    }
    Some(text)
}

fn parse_report(body: &[u8]) -> Option<Report> {
    let mut state = None;
    let mut id = None;
    let mut kind = None;
    let mut progress = None;
    let mut app = None;
    let mut title = None;
    let mut msg = None;
    for pair in body.split(|byte| *byte == b':') {
        let Some(equal) = pair.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let key = pair[..equal].trim_ascii();
        let value = pair[equal + 1..].trim_ascii();
        if key.len() > 16 {
            return None;
        }
        if key.is_empty() || !key.iter().all(u8::is_ascii_lowercase) {
            continue;
        }
        let limit = match key {
            b"msg" => 2732,
            b"title" => 256,
            b"app" => 32,
            b"id" => 128,
            _ => MAX_SEQUENCE,
        };
        if value.len() > limit {
            return None;
        }
        if !value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.,+/=-".contains(byte))
        {
            continue;
        }
        let value = std::str::from_utf8(value).ok()?;
        match key {
            b"state" => {
                state = match value {
                    "idle" => Some(ProgramState::Idle),
                    "working" => Some(ProgramState::Working),
                    "done" => Some(ProgramState::Done),
                    "blocked" => Some(ProgramState::Blocked),
                    "error" => Some(ProgramState::Error),
                    "clear" => Some(ProgramState::Clear),
                    _ => None,
                }
            }
            b"id" => id = Some(value.to_owned()),
            b"kind" => {
                kind = match value {
                    "permission" => Some(ProgramStatusKind::Permission),
                    "question" => Some(ProgramStatusKind::Question),
                    "auth" => Some(ProgramStatusKind::Auth),
                    _ => None,
                }
            }
            b"progress" => progress = value.parse::<u8>().ok().filter(|n| *n <= 100),
            b"app" => app = valid_token(value).then(|| value.to_owned()),
            b"title" => title = Some(decode_text(value, 192)?),
            b"msg" => msg = Some(decode_text(value, 2048)?),
            _ => {}
        }
    }
    let state = state?;
    if id.as_deref().is_some_and(|id| !valid_id(id)) {
        return None;
    }
    if state != ProgramState::Blocked {
        kind = None;
    }
    if !matches!(state, ProgramState::Working | ProgramState::Blocked) {
        progress = None;
    }
    Some(Report {
        state,
        id,
        kind,
        progress,
        app,
        title,
        msg,
    })
}

#[derive(Default)]
enum ScanState {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
    OtherString,
    OtherEscape,
}

/// Bounded scanner for OSCs beside the VTE parser. DCS and other strings stay
/// opaque, so a passthrough wrapper cannot forge a local status report.
#[derive(Default)]
pub(crate) struct Scanner {
    state: ScanState,
    raw: Vec<u8>,
    bytes: usize,
    oversized: bool,
}

impl Scanner {
    fn start(&mut self) {
        self.state = ScanState::Osc;
        self.raw.clear();
        self.bytes = 2;
        self.oversized = false;
    }

    fn count_byte(&mut self) {
        self.bytes += 1;
        if self.bytes > MAX_SEQUENCE {
            self.raw.clear();
            self.oversized = true;
        }
    }

    fn finish(&mut self) -> Option<Event> {
        self.state = ScanState::Ground;
        if self.oversized {
            self.raw.clear();
            return None;
        }
        let event = if self.raw == b"7501;?" {
            Some(Event::Query)
        } else {
            self.raw
                .strip_prefix(b"7501;")
                .and_then(parse_report)
                .map(Event::Report)
        };
        self.raw.clear();
        event
    }

    pub(crate) fn advance(&mut self, byte: u8) -> Option<Event> {
        match self.state {
            ScanState::Ground => {
                if byte == 0x1b {
                    self.state = ScanState::Escape;
                }
            }
            ScanState::Escape => match byte {
                b']' => self.start(),
                b'P' | b'X' | b'^' | b'_' => self.state = ScanState::OtherString,
                b'c' => {
                    self.state = ScanState::Ground;
                    return Some(Event::Reset);
                }
                0x1b => {}
                _ => self.state = ScanState::Ground,
            },
            ScanState::OtherString => match byte {
                0x1b => self.state = ScanState::OtherEscape,
                0x18 | 0x1a => self.state = ScanState::Ground,
                _ => {}
            },
            ScanState::OtherEscape => match byte {
                b'\\' | 0x18 | 0x1a => self.state = ScanState::Ground,
                0x1b => {}
                _ => self.state = ScanState::OtherString,
            },
            ScanState::Osc => {
                self.count_byte();
                match byte {
                    0x07 => return self.finish(),
                    0x1b => self.state = ScanState::OscEscape,
                    0x18 | 0x1a => {
                        self.state = ScanState::Ground;
                        self.raw.clear();
                    }
                    _ if !self.oversized => self.raw.push(byte),
                    _ => {}
                }
            }
            ScanState::OscEscape => {
                self.count_byte();
                if byte == b'\\' {
                    return self.finish();
                }
                self.state = ScanState::Escape;
                self.raw.clear();
                if byte == b']' {
                    self.start();
                } else if matches!(byte, b'P' | b'X' | b'^' | b'_') {
                    self.state = ScanState::OtherString;
                } else if byte != 0x1b {
                    self.state = ScanState::Ground;
                }
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RootStatus {
    pub state: ProgramState,
    pub kind: Option<ProgramStatusKind>,
}

/// One pane's root record and ordered transition facts. `revision` also lets a
/// lagging UI notice a clear that was followed by another report in one read.
/// A Working turn stays active through Blocked until it becomes Idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProgramStatusSnapshot {
    pub root: Option<RootStatus>,
    pub revision: u64,
    pub clear_revision: u64,
    pub working_to_idle_revision: u64,
    working_active: bool,
}

impl ProgramStatusSnapshot {
    pub fn apply(&mut self, report: Report) {
        if report.id.is_some() {
            return;
        }
        if report.state == ProgramState::Clear {
            self.clear();
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        if self.working_active && report.state == ProgramState::Idle {
            self.working_to_idle_revision = self.revision;
        }
        match report.state {
            ProgramState::Working => self.working_active = true,
            ProgramState::Blocked => {}
            ProgramState::Idle | ProgramState::Done | ProgramState::Error | ProgramState::Clear => {
                self.working_active = false;
            }
        }
        self.root = Some(RootStatus {
            state: report.state,
            kind: report.kind,
        });
    }

    pub fn clear(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.clear_revision = self.revision;
        self.root = None;
        self.working_active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, ProgramState, ProgramStatusKind, ProgramStatusSnapshot, Scanner};

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut scanner = Scanner::default();
        bytes
            .iter()
            .filter_map(|byte| scanner.advance(*byte))
            .collect()
    }

    #[test]
    fn split_frames_and_both_terminators() {
        for terminator in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
            let mut wire = b"\x1b]7501;state=blocked:kind=question:progress=50:app=claude-code:id=build/test:title=VGVzdA==:msg=T0s=".to_vec();
            wire.extend_from_slice(terminator);
            let expected = events(&wire);
            assert_eq!(expected.len(), 1);
            for split in 0..wire.len() {
                let mut scanner = Scanner::default();
                let mut found = Vec::new();
                for part in [&wire[..split], &wire[split..]] {
                    found.extend(part.iter().filter_map(|byte| scanner.advance(*byte)));
                }
                assert_eq!(found, expected, "split at {split}");
            }
            let Event::Report(report) = &expected[0] else {
                panic!("expected report");
            };
            assert_eq!(report.state, ProgramState::Blocked);
            assert_eq!(report.kind, Some(ProgramStatusKind::Question));
            assert_eq!(report.progress, Some(50));
            assert_eq!(report.id.as_deref(), Some("build/test"));
            assert_eq!(report.app.as_deref(), Some("claude-code"));
            assert_eq!(report.title.as_deref(), Some("Test"));
            assert_eq!(report.msg.as_deref(), Some("OK"));
        }
    }

    #[test]
    fn rejects_bad_or_oversize_reports_and_resynchronizes() {
        for body in [
            "state=unknown",
            "state=working:id=bad//id",
            "state=working:msg=abcde",
            "state=working:msg=SGVsbG8K",
            "state=working:title=/w==",
            "state=working:msg=//8=",
        ] {
            let wire = format!("\x1b]7501;{body}\x07");
            assert!(events(wire.as_bytes()).is_empty(), "{body}");
        }
        let mut wire = format!("\x1b]7501;state=working:msg={}\x07", "A".repeat(4100)).into_bytes();
        wire.extend_from_slice(b"\x1b]7501;?\x07");
        assert_eq!(events(&wire), vec![Event::Query]);
        assert_eq!(
            events(b"\x1b]7501;?\x1bq\x1b]7501;?\x07"),
            vec![Event::Query]
        );
        assert!(events(b"\x1b]7501;?\x1bX\x1b]7501;?\x07\x1b\\").is_empty());
        assert!(events(b"\x1bPopaque\x1b]7501;?\x07\x1b\\").is_empty());
        assert_eq!(events(b"\x1bc"), vec![Event::Reset]);
    }

    #[test]
    fn last_duplicate_wins_and_optional_values_are_ignored() {
        let found = events(b"\x1b]7501;state=working:state=blocked:kind=unknown:progress=101:app=bad/name:unknown=x:msg=T0s:ignored=@@@\x07");
        let [Event::Report(report)] = found.as_slice() else {
            panic!("expected one report");
        };
        assert_eq!(report.state, ProgramState::Blocked);
        assert_eq!(report.kind, None);
        assert_eq!(report.progress, None);
        assert_eq!(report.app, None);
        assert_eq!(report.msg.as_deref(), Some("OK"));
    }

    #[test]
    fn root_clear_and_child_reports_do_not_override_the_pane() {
        let mut status = ProgramStatusSnapshot::default();
        for event in events(b"\x1b]7501;state=working\x07\x1b]7501;state=blocked:id=worker:kind=permission\x07\x1b]7501;state=blocked:kind=permission\x07\x1b]7501;state=idle\x07") {
            if let Event::Report(report) = event {
                status.apply(report);
            }
        }
        assert_eq!(status.root.unwrap().state, ProgramState::Idle);
        assert_eq!(status.working_to_idle_revision, status.revision);
        let clear = events(b"\x1b]7501;state=clear\x07").pop().unwrap();
        if let Event::Report(report) = clear {
            status.apply(report);
        }
        assert_eq!(status.root, None);
        assert_eq!(status.clear_revision, status.revision);
    }
}
