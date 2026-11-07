//! Which links a web page in a browser pane may open in a new tab.
//!
//! A page asks for a new tab two ways. The webview reports a request for a new
//! window (a `target=_blank` click, `window.open`), which muxel opens in front.
//! muxel's own page script reports a middle-click or Ctrl+click on a link,
//! which muxel opens in the background. The page can run code that sends the
//! same message, so the host decides what such a message may do: it carries
//! the view's random token, it names a web page, and it arrives no faster than
//! a person clicks. Anything else is dropped.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Whether `url` is a web page. A page may open only these: not a local file,
/// which would land in an editor tab, and not another scheme the OS hands to
/// whichever program registered it.
pub fn is_web_url(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    })
}

/// How muxel's page script asks for a background tab: this prefix, the view's
/// token, a colon, and the link's URL.
pub const BACKGROUND_OPEN: &str = "muxel:open-background:";

/// The message asking for a background tab on `url`, from the view with
/// `token`. The host builds this itself for a link opened from its own menu.
pub fn background_open_message(token: &str, url: &str) -> String {
    format!("{BACKGROUND_OPEN}{token}:{url}")
}

/// Most background tabs one page opens at once.
pub const BURST: u32 = 10;

/// After a burst, a page opens one background tab per interval, about three a
/// second, which is faster than a person middle-clicks.
pub const INTERVAL: Duration = Duration::from_millis(333);

/// How long a background open's URL is remembered, to drop a new-window
/// request for the same link if the webview raises one anyway.
pub const TWIN_WINDOW: Duration = Duration::from_secs(2);

/// One browser view's background opens: its token, how many it may still open
/// now, and the URLs it opened lately.
#[derive(Debug)]
pub struct PageOpens {
    token: String,
    /// When the budget is next full again, in the generic cell rate
    /// algorithm's terms: each open moves it on by [`INTERVAL`], and an open
    /// is refused while it is more than a burst ahead of now.
    full_at: Option<Instant>,
    recent: VecDeque<(Instant, String)>,
}

impl PageOpens {
    /// The view's opens, for messages carrying `token`. The token is a random
    /// value only the view's own page script knows.
    pub fn new(token: String) -> Self {
        Self {
            token,
            full_at: None,
            recent: VecDeque::new(),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The URL `message`, received at `at`, may open in a background tab.
    /// `None`, and nothing spent, unless it is a background open with this
    /// view's token naming a web page. `None` too when the page has opened
    /// a burst already and the budget hasn't come back; the open is dropped,
    /// not queued.
    pub fn accept<'m>(&mut self, message: &'m str, at: Instant) -> Option<&'m str> {
        let (token, url) = message.strip_prefix(BACKGROUND_OPEN)?.split_once(':')?;
        if token != self.token || !is_web_url(url) || !self.spend(at) {
            return None;
        }
        self.recent.push_back((at, url.to_string()));
        Some(url)
    }

    /// Whether `url` opened in the background within [`TWIN_WINDOW`] of
    /// `now`. A new-window request for it then comes from the same click.
    pub fn opened_lately(&mut self, url: &str, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > TWIN_WINDOW)
        {
            self.recent.pop_front();
        }
        self.recent.iter().any(|(_, opened)| opened == url)
    }

    fn spend(&mut self, at: Instant) -> bool {
        let full_at = self.full_at.map_or(at, |full_at| full_at.max(at));
        if full_at.saturating_duration_since(at) > INTERVAL * (BURST - 1) {
            return false;
        }
        self.full_at = Some(full_at + INTERVAL);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{BURST, INTERVAL, PageOpens, TWIN_WINDOW, background_open_message, is_web_url};
    use std::time::{Duration, Instant};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn opens() -> PageOpens {
        PageOpens::new(TOKEN.to_string())
    }

    fn ask(url: &str) -> String {
        background_open_message(TOKEN, url)
    }

    #[test]
    fn only_web_pages_count_as_web_urls() {
        assert!(is_web_url("https://example.com/new"));
        assert!(is_web_url("HTTP://example.com/new"));
        for url in [
            "file:///C:/Users/test/secret.txt",
            "mailto:test@example.com",
            "custom-protocol:payload",
            "javascript:alert(1)",
            "//example.com/scheme-relative",
            "C:/Users/test/secret.txt",
            "",
        ] {
            assert!(!is_web_url(url), "{url:?}");
        }
    }

    #[test]
    fn a_message_with_the_views_token_opens_its_web_link() {
        let mut opens = opens();
        let message = ask("https://example.com/a?b=1#c");
        assert_eq!(
            opens.accept(&message, Instant::now()),
            Some("https://example.com/a?b=1#c")
        );
    }

    #[test]
    fn a_message_without_the_views_token_opens_nothing() {
        let mut opens = opens();
        let now = Instant::now();
        for message in [
            "muxel:open-background:https://example.com/".to_string(),
            background_open_message("", "https://example.com/"),
            background_open_message("00000000000000000000000000000000", "https://example.com/"),
            background_open_message(&TOKEN[1..], "https://example.com/"),
            background_open_message(&format!("{TOKEN}0"), "https://example.com/"),
            format!("{TOKEN}:https://example.com/"),
            "https://example.com/".to_string(),
        ] {
            assert_eq!(opens.accept(&message, now), None, "{message:?}");
        }
    }

    #[test]
    fn a_page_never_opens_a_local_file_or_another_scheme() {
        let mut opens = opens();
        let now = Instant::now();
        for url in [
            "file:///C:/Windows/win.ini",
            "FILE://server/share/x.md",
            "C:/Windows/win.ini",
            "/etc/passwd",
            "javascript:alert(1)",
            "ms-settings:privacy",
            "mailto:someone@example.com",
            "muxel:anything",
            "",
        ] {
            assert_eq!(opens.accept(&ask(url), now), None, "{url:?}");
        }
    }

    #[test]
    fn a_burst_opens_then_a_few_a_second_and_the_rest_drop() {
        let mut opens = opens();
        let start = Instant::now();
        let link = ask("https://example.com/");
        let opened = (0..20)
            .filter(|_| opens.accept(&link, start).is_some())
            .count();
        assert_eq!(opened, BURST as usize);
        // A second later about three have come back, and no more.
        let later = start + Duration::from_secs(1);
        let opened = (0..20)
            .filter(|_| opens.accept(&link, later).is_some())
            .count();
        assert_eq!(opened, 3);
        // Clicking at a person's pace never runs out.
        let mut at = later + INTERVAL * BURST;
        for _ in 0..50 {
            assert!(opens.accept(&link, at).is_some());
            at += INTERVAL;
        }
    }

    #[test]
    fn refused_messages_spend_nothing() {
        let mut opens = opens();
        let now = Instant::now();
        for _ in 0..100 {
            opens.accept("muxel:open-background:forged:https://example.com/", now);
            opens.accept(&ask("file:///C:/Windows/win.ini"), now);
        }
        let link = ask("https://example.com/");
        let opened = (0..BURST)
            .filter(|_| opens.accept(&link, now).is_some())
            .count();
        assert_eq!(opened, BURST as usize);
    }

    #[test]
    fn a_new_window_request_twins_a_background_open_only_briefly() {
        let mut opens = opens();
        let at = Instant::now();
        opens.accept(&ask("https://example.com/a"), at);
        assert!(opens.opened_lately("https://example.com/a", at + Duration::from_millis(900)));
        assert!(!opens.opened_lately("https://example.com/b", at));
        assert!(!opens.opened_lately(
            "https://example.com/a",
            at + TWIN_WINDOW + Duration::from_millis(1)
        ));
    }
}
