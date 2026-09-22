//! Forward the configured action-sheet chord from WebView2's native input
//! surface. The COM callback only admits a bounded request; GPUI owns execution.

use gpui::{App, Global, Keystroke, Modifiers};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Chord {
    key: u32,
    modifiers: Modifiers,
}

#[derive(Clone, Default)]
pub struct Shortcut(Arc<RwLock<Option<Chord>>>);
impl Global for Shortcut {}

pub struct Request {
    pub focus: isize,
    received: Instant,
    chord: Chord,
}

fn request_is_current(
    request: &Request,
    now: Instant,
    focus: isize,
    current_chord: Option<&Chord>,
) -> bool {
    now.saturating_duration_since(request.received) <= Duration::from_millis(250)
        && focus == request.focus
        && current_chord == Some(&request.chord)
}

fn chord(source: &str) -> Option<Chord> {
    if source.split_whitespace().count() != 1 {
        return None;
    }
    let keystroke = Keystroke::parse(source).ok()?;
    let key = match keystroke.key.as_str() {
        "space" => 0x20,
        "tab" => 0x09,
        "enter" => 0x0d,
        "escape" => 0x1b,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "insert" => 0x2d,
        "delete" => 0x2e,
        "backspace" => 0x08,
        key if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() => {
            key.as_bytes()[0].to_ascii_uppercase() as u32
        }
        key => {
            let function = key.strip_prefix('f')?.parse::<u32>().ok()?;
            if !(1..=24).contains(&function) {
                return None;
            }
            0x70 + function - 1
        }
    };
    Some(Chord {
        key,
        modifiers: keystroke.modifiers,
    })
}

pub fn install(settings: &muxel_core::Settings, cx: &mut App) {
    let shortcut = cx.try_global::<Shortcut>().cloned().unwrap_or_default();
    let current = crate::action_catalog::effective_bindings(settings)
        .into_iter()
        .find(|binding| binding.action == "ShowActionSheet")
        .and_then(|binding| chord(binding.keystroke));
    *shortcut.0.write().unwrap() = current;
    cx.set_global(shortcut);
}

impl Shortcut {
    pub fn from_app(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    pub fn capture(&self, key: u32) -> Option<Request> {
        let chord = self.0.read().ok()?.clone()?;
        // This is called on the owning WebView2 UI thread during its key event.
        let down = |key| unsafe { GetKeyState(key) < 0 };
        let modifiers = Modifiers {
            control: down(VK_CONTROL.0 as i32),
            alt: down(VK_MENU.0 as i32),
            shift: down(VK_SHIFT.0 as i32),
            platform: down(VK_LWIN.0 as i32) || down(VK_RWIN.0 as i32),
            function: false,
        };
        if chord.key != key || chord.modifiers != modifiers {
            return None;
        }
        let focus = unsafe { GetFocus() }.0;
        if focus == 0 {
            return None;
        }
        Some(Request {
            focus,
            received: Instant::now(),
            chord,
        })
    }

    pub fn current(&self, request: &Request) -> bool {
        let now = Instant::now();
        let focus = unsafe { GetFocus() }.0;
        self.0
            .read()
            .is_ok_and(|chord| request_is_current(request, now, focus, chord.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Request, chord, request_is_current};
    use std::time::{Duration, Instant};
    #[test]
    fn native_prefix_tracks_supported_single_chords_without_guessing_layout_punctuation() {
        let default = chord("ctrl-shift-space").unwrap();
        assert_eq!(default.key, 0x20);
        assert!(default.modifiers.control && default.modifiers.shift);
        assert_eq!(chord("ctrl-shift-x").unwrap().key, b'X' as u32);
        assert!(chord("ctrl-k ctrl-s").is_none());
        assert!(chord("ctrl-;").is_none());
    }

    #[test]
    fn native_request_expires_on_age_focus_or_rebind() {
        let received = Instant::now();
        let original = chord("ctrl-shift-space").unwrap();
        let rebound = chord("ctrl-shift-x").unwrap();
        let request = Request {
            focus: 42,
            received,
            chord: original.clone(),
        };

        assert!(request_is_current(
            &request,
            received + Duration::from_millis(250),
            42,
            Some(&original),
        ));
        assert!(!request_is_current(
            &request,
            received + Duration::from_millis(251),
            42,
            Some(&original),
        ));
        assert!(!request_is_current(&request, received, 43, Some(&original),));
        assert!(!request_is_current(&request, received, 42, Some(&rebound),));
    }
}
