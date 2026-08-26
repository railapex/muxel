//! The built-in browser pane.
//!
//! macOS/Windows: an embedded system webview (WKWebView / WebView2) hosted as a
//! native child of the gpui window via `gpui-wry`, with a small address-bar
//! toolbar rendered above it. **The native webview always draws on top of gpui
//! content inside its bounds** — the app is responsible for calling
//! [`BrowserView::set_native_visible`] so it hides whenever an overlay/modal
//! covers it or its tab isn't the active one (see `MuxelApp::sync_browser_visibility`).
//!
//! Linux: gpui can't host a WebKitGTK child (no GTK loop; XEmbed is X11-only),
//! so there is no embedded pane — links open in a separate muxel-managed
//! WebKitGTK window instead (`browser_helper.rs`). The `BrowserView` here is a
//! placeholder shown only if a workspace synced from another OS contains
//! browser panes.

use crate::i18n::t;
use gpui::*;
use gpui_component::{ActiveTheme as _, v_flex};

/// Native browser state that can be reused when creating another tab. On
/// Windows this avoids rebuilding the WebView2 environment for every tab.
#[derive(Clone, Default)]
pub struct BrowserLaunchContext {
    #[cfg(target_os = "windows")]
    environment: Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment>,
}

/// Spawn the separate Linux browser window (`muxel --browser <url>`), returning
/// whether the helper was launched (false → caller falls back to the OS browser).
#[cfg(target_os = "linux")]
pub fn spawn_browser_window(url: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .arg("--browser")
        .arg(url)
        .spawn()
        .is_ok()
}

/// A short label for a browser tab: the URL's host (falls back to the URL).
fn tab_label(url: &str) -> String {
    let url_without_fragment = url.split('#').next().unwrap_or(url);
    if let Some(name) = muxel_terminal::path_from_file_uri(url_without_fragment)
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
    {
        return name;
    }

    let trimmed = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = trimmed.split(['/', '?']).next().unwrap_or(trimmed);
    if host.is_empty() {
        t("Browser").to_string()
    } else {
        host.to_string()
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn should_publish_url(changed: bool, navigation_committed: bool) -> bool {
    changed || navigation_committed
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeFocusEvent {
    Gained,
    Lost,
    Pointer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeFocusDecision {
    Activate,
    RestoreActive,
}

/// Only a real page pointer may activate a visible-but-inactive browser. Native
/// controllers can report GotFocus later without a click (notably while an
/// async WebView2 controller finishes), so treating every gain as intent lets a
/// browser steal focus back from a terminal. An already-active browser may
/// still accept GotFocus to settle address-bar/parent focus bookkeeping.
fn native_focus_decision(
    native_visible: bool,
    pane_active: bool,
    events: &[NativeFocusEvent],
) -> Option<NativeFocusDecision> {
    let mut focused = None;
    let mut trusted_pointer = false;
    for event in events {
        match event {
            NativeFocusEvent::Gained => focused = Some(true),
            NativeFocusEvent::Lost => {
                focused = Some(false);
                trusted_pointer = false;
            }
            NativeFocusEvent::Pointer => {
                focused = Some(true);
                trusted_pointer = true;
            }
        }
    }
    match focused {
        Some(true) if !native_visible => Some(NativeFocusDecision::RestoreActive),
        Some(true) if pane_active || trusted_pointer => Some(NativeFocusDecision::Activate),
        Some(true) => Some(NativeFocusDecision::RestoreActive),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NativeFocusDecision, NativeFocusEvent, native_focus_decision, should_publish_url, tab_label,
    };

    #[test]
    fn local_file_tab_uses_decoded_filename() {
        assert_eq!(
            tab_label("file:///D:/business/report%202026.html#L12"),
            "report 2026.html"
        );
    }

    #[test]
    fn a_committed_requested_navigation_refreshes_workspace_metadata() {
        assert!(should_publish_url(false, true));
        assert!(should_publish_url(true, false));
        assert!(!should_publish_url(false, false));
    }

    #[test]
    fn native_focus_requires_visibility_and_current_intent() {
        use NativeFocusDecision::{Activate, RestoreActive};
        use NativeFocusEvent::{Gained, Lost, Pointer};

        assert_eq!(
            native_focus_decision(false, false, &[Gained]),
            Some(RestoreActive)
        );
        assert_eq!(
            native_focus_decision(true, false, &[Gained]),
            Some(RestoreActive)
        );
        assert_eq!(native_focus_decision(true, true, &[Gained]), Some(Activate));
        assert_eq!(
            native_focus_decision(true, false, &[Pointer]),
            Some(Activate)
        );
        assert_eq!(
            native_focus_decision(true, false, &[Gained, Pointer]),
            Some(Activate)
        );
        assert_eq!(
            native_focus_decision(true, false, &[Pointer, Gained]),
            Some(Activate)
        );
        assert_eq!(native_focus_decision(true, false, &[Pointer, Lost]), None);
        assert_eq!(native_focus_decision(false, false, &[Lost]), None);
    }
}

// ---------------------------------------------------------------------------
// macOS / Windows: the real embedded webview pane.
// ---------------------------------------------------------------------------
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use super::*;
    use gpui_component::button::{Button, ButtonVariants as _};
    use gpui_component::input::{
        Input, InputEvent, InputState, MoveToEnd, MoveToStart, SelectToStart,
    };
    use gpui_component::{Icon, IconName, Sizable as _, h_flex};
    #[cfg(target_os = "windows")]
    use webview2_com::{CoTaskMemPWSTR, FocusChangedEventHandler, WebMessageReceivedEventHandler};
    use wry::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[cfg(target_os = "windows")]
    use wry::{WebViewBuilderExtWindows as _, WebViewExtWindows as _};

    #[cfg(target_os = "windows")]
    enum SharedEnvironment {
        Empty,
        Building(
            Vec<
                async_channel::Sender<
                    Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment>,
                >,
            >,
        ),
        Ready(webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment),
    }

    #[cfg(target_os = "windows")]
    thread_local! {
        /// One WebView2 environment per process/profile. Browser views are built
        /// by interleaved foreground tasks; the Building marker prevents two
        /// restored split panes from racing separate cold environments against
        /// the same user-data directory.
        static SHARED_ENVIRONMENT: std::cell::RefCell<SharedEnvironment> =
            const { std::cell::RefCell::new(SharedEnvironment::Empty) };
    }

    #[cfg(target_os = "windows")]
    enum EnvironmentClaim {
        Ready(webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment),
        Build,
        Wait(
            async_channel::Receiver<
                Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment>,
            >,
        ),
    }

    #[cfg(target_os = "windows")]
    fn claim_environment() -> EnvironmentClaim {
        SHARED_ENVIRONMENT.with(|state| {
            let mut state = state.borrow_mut();
            match &mut *state {
                SharedEnvironment::Ready(environment) => {
                    EnvironmentClaim::Ready(environment.clone())
                }
                SharedEnvironment::Building(waiters) => {
                    let (sender, receiver) = async_channel::bounded(1);
                    waiters.push(sender);
                    EnvironmentClaim::Wait(receiver)
                }
                SharedEnvironment::Empty => {
                    *state = SharedEnvironment::Building(Vec::new());
                    EnvironmentClaim::Build
                }
            }
        })
    }

    #[cfg(target_os = "windows")]
    struct EnvironmentBuildGuard {
        armed: bool,
    }

    #[cfg(target_os = "windows")]
    fn finish_environment(
        environment: Option<
            webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
        >,
    ) {
        let waiters = SHARED_ENVIRONMENT.with(|state| {
            let next = environment
                .clone()
                .map_or(SharedEnvironment::Empty, SharedEnvironment::Ready);
            match std::mem::replace(&mut *state.borrow_mut(), next) {
                SharedEnvironment::Building(waiters) => waiters,
                _ => Vec::new(),
            }
        });
        for waiter in waiters {
            let _ = waiter.try_send(environment.clone());
        }
    }

    #[cfg(target_os = "windows")]
    impl EnvironmentBuildGuard {
        fn new() -> Self {
            Self { armed: true }
        }

        fn finish(
            mut self,
            environment: Option<
                webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Environment,
            >,
        ) {
            finish_environment(environment);
            self.armed = false;
        }
    }

    #[cfg(target_os = "windows")]
    impl Drop for EnvironmentBuildGuard {
        fn drop(&mut self) {
            if self.armed {
                finish_environment(None);
            }
        }
    }

    /// The gpui window's native handle, detached from the `&Window` it came from.
    ///
    /// The webview is built from a spawned task (see [`BrowserView::new`]), where
    /// no `&Window` is in scope, so wry is handed this instead.
    struct ParentWindow(RawWindowHandle);

    impl HasWindowHandle for ParentWindow {
        fn window_handle(
            &self,
        ) -> Result<wry::raw_window_handle::WindowHandle<'_>, wry::raw_window_handle::HandleError>
        {
            // SAFETY: the handle belongs to the gpui window hosting this pane,
            // which outlives the pane and therefore the task borrowing it.
            Ok(unsafe { wry::raw_window_handle::WindowHandle::borrow_raw(self.0) })
        }
    }

    const CLICK_MSG_PLACEHOLDER: &str = "__MUXEL_PAGE_CLICK_MESSAGE__";

    /// Injected into every page (and every frame) before its own scripts run.
    ///
    /// The native webview is a real child window stacked ABOVE gpui, so a click
    /// inside it is consumed by the OS and gpui never dispatches a mouse event for
    /// it — which is why the pane's `on_mouse_down` handler (app.rs) can't see it,
    /// and why the clicked pane never became the active one. The page itself is the
    /// only thing that *can* see the click, so it tells us. Capture phase, so a
    /// page that stops propagation on its own handlers can't hide the click.
    const PAGE_CLICK_SCRIPT: &str = r#"
        (function () {
          window.addEventListener('mousedown', function (event) {
            if (event.button !== 0 || !event.isTrusted) return;
            try {
              if (window.chrome && window.chrome.webview) {
                window.chrome.webview.postMessage('__MUXEL_PAGE_CLICK_MESSAGE__');
              } else if (window.ipc &&
                         (window.location.protocol === 'http:' ||
                          window.location.protocol === 'https:')) {
                window.ipc.postMessage('__MUXEL_PAGE_CLICK_MESSAGE__');
              }
            } catch (e) {}
          }, true);
        })();
    "#;

    const COPY_SCRIPT: &str = r#"
        (function () {
          // WebView2 hosted as a child window does not always run its built-in
          // copy accelerator. The native context-menu command still works, so
          // invoke that same document copy operation from the standard keys.
          window.addEventListener('keydown', function (event) {
            const key = String(event.key || '').toLowerCase();
            const copy = !event.altKey && !event.shiftKey && (
              ((event.ctrlKey || event.metaKey) && key === 'c') ||
              (event.ctrlKey && (key === 'insert' || event.code === 'Insert'))
            );
            if (copy) {
              try {
                if (document.execCommand('copy')) {
                  event.preventDefault();
                  event.stopPropagation();
                }
              } catch (e) {}
            }
          }, true);
        })();
    "#;

    enum NativeBrowserEvent {
        OpenTab(String),
    }

    pub struct BrowserView {
        focus_handle: FocusHandle,
        webview: Option<Entity<gpui_wry::WebView>>,
        /// Set once the deferred build finished *and* failed, so `render` can
        /// tell "still starting" apart from "gave up".
        webview_failed: bool,
        address: Entity<InputState>,
        url: String,
        /// Previous URL while a requested navigation has not committed yet.
        /// WebView2 reports the old page during that gap; do not sync it back.
        pending_navigation_from: Option<String>,
        /// What the app last asked of the native child (dedupes plaform calls).
        native_visible: bool,
        /// Native page-focus events. WebView2 reports these from its controller;
        /// macOS uses guarded page IPC until WKWebView exposes the same seam.
        focus_events: std::sync::mpsc::Receiver<NativeFocusEvent>,
    }

    impl BrowserView {
        pub fn new(
            instance_id: uuid::Uuid,
            url: String,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            Self::new_with_options(
                instance_id,
                url,
                BrowserLaunchContext::default(),
                true,
                window,
                cx,
            )
        }

        pub fn new_with_options(
            instance_id: uuid::Uuid,
            url: String,
            launch_context: BrowserLaunchContext,
            initially_visible: bool,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let address = cx.new(|cx| InputState::new(window, cx).default_value(url.clone()));
            cx.subscribe_in(
                &address,
                window,
                |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Focus => {
                        let handle = input.read(cx).focus_handle(cx);
                        // Select end-to-start so the whole URL is selected while
                        // the scheme and host remain at the visible edge.
                        handle.dispatch_action(&MoveToEnd, window, cx);
                        handle.dispatch_action(&SelectToStart, window, cx);
                    }
                    InputEvent::PressEnter { .. } => {
                        let typed = input.read(cx).value().trim().to_string();
                        if !typed.is_empty() {
                            this.navigate(&muxel_core::normalize_url(&typed), cx);
                            // Navigation is done. Return GPUI focus to the pane,
                            // then hand OS keyboard focus back to the page.
                            this.focus_handle.focus(window, cx);
                            this.focus_native(cx);
                        }
                    }
                    InputEvent::Blur => {
                        // An unfocused address bar should show its scheme/host,
                        // not remain scrolled to the tail where editing ended.
                        input
                            .read(cx)
                            .focus_handle(cx)
                            .dispatch_action(&MoveToStart, window, cx);
                    }
                    _ => {}
                },
            )
            .detach();

            // Build the native webview from a spawned task, never inline here.
            //
            // WebView2 (Windows) initialises its controller by running a nested
            // Win32 message pump. Called inline, that pump re-enters gpui while
            // `App`'s RefCell is still mutably borrowed by the update building
            // this view, so the first foreground task it happens to run — a
            // terminal's PTY reader, say — panics with "RefCell already
            // borrowed". The async builder awaits a completion handler rather
            // than pumping, and a task body holds no borrow before its first
            // `update`, so neither hazard applies.
            //
            // Failure (e.g. the WebView2 runtime is missing) degrades to a
            // visible error row instead of crashing the pane.
            //
            // `HasWindowHandle::window_handle` is spelled out because gpui's
            // inherent `Window::window_handle` otherwise wins and yields gpui's
            // own `AnyWindowHandle`.
            let parent = HasWindowHandle::window_handle(&*window)
                .ok()
                .map(|h| ParentWindow(h.as_raw()));
            let requested = url.clone();
            let build_queued = std::time::Instant::now();
            let (focus_tx, focus_events) = std::sync::mpsc::channel();
            let (native_event_tx, native_events) = async_channel::unbounded();

            cx.spawn_in(window, async move |this, cx| {
                if crate::ui_profile::is_enabled() {
                    crate::ui_profile::event(
                        "browser",
                        &format!(
                            "native-controller-task pane={instance_id} initially_visible={initially_visible}"
                        ),
                    );
                }
                #[cfg(not(target_os = "windows"))]
                let _ = launch_context;
                #[cfg(target_os = "windows")]
                let mut shared_environment = launch_context.environment;
                #[cfg(target_os = "windows")]
                let mut environment_build_guard = None;
                #[cfg(target_os = "windows")]
                if shared_environment.is_none() {
                    loop {
                        match claim_environment() {
                            EnvironmentClaim::Ready(environment) => {
                                if crate::ui_profile::is_enabled() {
                                    crate::ui_profile::event(
                                        "browser",
                                        &format!("environment-ready pane={instance_id}"),
                                    );
                                }
                                shared_environment = Some(environment);
                                break;
                            }
                            EnvironmentClaim::Build => {
                                if crate::ui_profile::is_enabled() {
                                    crate::ui_profile::event(
                                        "browser",
                                        &format!("environment-build pane={instance_id}"),
                                    );
                                }
                                environment_build_guard = Some(EnvironmentBuildGuard::new());
                                break;
                            }
                            EnvironmentClaim::Wait(receiver) => {
                                if crate::ui_profile::is_enabled() {
                                    crate::ui_profile::event(
                                        "browser",
                                        &format!("environment-wait pane={instance_id}"),
                                    );
                                }
                                if let Ok(Some(environment)) = receiver.recv().await {
                                    if crate::ui_profile::is_enabled() {
                                        crate::ui_profile::event(
                                            "browser",
                                            &format!("environment-woken pane={instance_id}"),
                                        );
                                    }
                                    shared_environment = Some(environment);
                                    break;
                                }
                            }
                        }
                    }
                }
                #[cfg(target_os = "windows")]
                let environment_kind = if environment_build_guard.is_some() {
                    "cold"
                } else {
                    "shared"
                };
                #[cfg(target_os = "macos")]
                let environment_kind = "platform";
                #[cfg(target_os = "windows")]
                let mut web_context = if shared_environment.is_none() {
                    muxel_store::data_dir().and_then(|dir| {
                        let dir = dir.join("webview2");
                        std::fs::create_dir_all(&dir)
                            .ok()
                            .map(|()| wry::WebContext::new(Some(dir)))
                    })
                } else {
                    None
                };

                let built = match parent.as_ref() {
                    Some(parent) => {
                        #[cfg(target_os = "windows")]
                        let Some(builder) = shared_environment
                            .map(|environment| {
                                wry::WebViewBuilder::new().with_environment(environment)
                            })
                            .or_else(|| {
                                web_context
                                    .as_mut()
                                    .map(wry::WebViewBuilder::new_with_web_context)
                            })
                        else {
                            let _ = this.update_in(cx, |this, _window, cx| {
                                this.webview_failed = true;
                                cx.notify();
                            });
                            return;
                        };
                        #[cfg(target_os = "macos")]
                        let builder = wry::WebViewBuilder::new();

                        // A per-view nonce makes this a user-pointer signal, not a
                        // public command arbitrary page JavaScript can replay by
                        // knowing Muxel's fixed source string.
                        let click_message = format!("muxel:page-click:{}", uuid::Uuid::new_v4());
                        let page_click_script =
                            PAGE_CLICK_SCRIPT.replace(CLICK_MSG_PLACEHOLDER, &click_message);
                        let open_tab_tx = native_event_tx.clone();
                        let builder = builder
                            .with_url(&requested)
                            .with_visible(initially_visible)
                            .with_initialization_script(&page_click_script)
                            .with_initialization_script(COPY_SCRIPT)
                            .with_new_window_req_handler(move |url, _features| {
                                // A dropped receiver means this BrowserView was already torn down;
                                // still deny the native popup so no orphan OS window escapes.
                                let _ = open_tab_tx.try_send(NativeBrowserEvent::OpenTab(url));
                                wry::NewWindowResponse::Deny
                            });
                        #[cfg(target_os = "macos")]
                        let builder = builder.with_ipc_handler(move |req| {
                            if req.body().as_str() == click_message {
                                let _ = focus_tx.send(NativeFocusEvent::Pointer);
                            }
                        });
                        let controller_started = std::time::Instant::now();
                        let queued_ms = build_queued.elapsed().as_millis();
                        if crate::ui_profile::is_enabled() {
                            crate::ui_profile::event(
                                "browser",
                                &format!(
                                    "native-controller-await pane={instance_id} env={environment_kind} initially_visible={initially_visible} queued_ms={queued_ms}"
                                ),
                            );
                        }
                        let built = builder.build_as_child_async(parent).await.ok();
                        #[cfg(target_os = "windows")]
                        if let Some(guard) = environment_build_guard.take() {
                            guard.finish(built.as_ref().map(|webview| webview.environment()));
                        }
                        if crate::ui_profile::is_enabled() {
                            crate::ui_profile::event(
                                "browser",
                                &format!(
                                    "native-controller pane={instance_id} env={environment_kind} initially_visible={initially_visible} queued_ms={queued_ms} controller_ms={} ok={}",
                                    controller_started.elapsed().as_millis(),
                                    built.is_some()
                                ),
                            );
                        }

                        #[cfg(target_os = "windows")]
                        if let Some(webview) = built.as_ref() {
                            // Wry's IPC wrapper parses the page URL as an
                            // `http::Uri`; a valid Windows file:/// URL can trip
                            // that conversion. Read WebView2's string message
                            // directly instead.
                            let pointer_tx = focus_tx.clone();
                            let pointer_message = click_message;
                            let web_message =
                                WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                                    if let Some(args) = args {
                                        let mut raw = windows_webview::core::PWSTR::null();
                                        let result =
                                            unsafe { args.TryGetWebMessageAsString(&mut raw) };
                                        let body = CoTaskMemPWSTR::from(raw).to_string();
                                        if result.is_ok() && body == pointer_message {
                                            let _ = pointer_tx.send(NativeFocusEvent::Pointer);
                                        }
                                    }
                                    Ok(())
                                }));
                            let mut message_token = 0;
                            if let Err(error) = unsafe {
                                webview
                                    .webview()
                                    .add_WebMessageReceived(&web_message, &mut message_token)
                            } {
                                log::warn!("failed to observe WebView2 page messages: {error}");
                            }

                            let got_tx = focus_tx.clone();
                            let got_focus =
                                FocusChangedEventHandler::create(Box::new(move |_, _| {
                                    let _ = got_tx.send(NativeFocusEvent::Gained);
                                    Ok(())
                                }));
                            let lost_tx = focus_tx.clone();
                            let lost_focus =
                                FocusChangedEventHandler::create(Box::new(move |_, _| {
                                    let _ = lost_tx.send(NativeFocusEvent::Lost);
                                    Ok(())
                                }));
                            let mut got_token = 0;
                            if let Err(error) = unsafe {
                                webview
                                    .controller()
                                    .add_GotFocus(&got_focus, &mut got_token)
                            } {
                                log::warn!("failed to observe WebView2 focus: {error}");
                            }
                            let mut lost_token = 0;
                            if let Err(error) = unsafe {
                                webview
                                    .controller()
                                    .add_LostFocus(&lost_focus, &mut lost_token)
                            } {
                                log::warn!("failed to observe WebView2 blur: {error}");
                            }
                        }

                        built
                    }
                    None => None,
                };

                let _ = this.update_in(cx, |this, window, cx| {
                    let Some(wv) = built else {
                        this.webview_failed = true;
                        cx.notify();
                        return;
                    };
                    let wv = cx.new(|cx2| gpui_wry::WebView::new(wv, window, cx2));
                    // Re-apply whatever the pane changed while the build ran.
                    let visible = this.native_visible;
                    wv.update(cx, |wv, _| if visible { wv.show() } else { wv.hide() });
                    if this.url != requested {
                        let current = this.url.clone();
                        wv.update(cx, |wv, _| wv.load_url(&current));
                    }
                    this.webview = Some(wv);
                    cx.notify();
                });

                drop(native_event_tx);
                while let Ok(NativeBrowserEvent::OpenTab(url)) = native_events.recv().await {
                    crate::app::open_browser_tab_from_native(instance_id, url, cx);
                }
            })
            .detach();

            Self {
                focus_handle: cx.focus_handle(),
                webview: None,
                webview_failed: false,
                address,
                url,
                pending_navigation_from: None,
                native_visible: initially_visible,
                focus_events,
            }
        }

        pub fn launch_context(&self, _cx: &App) -> Option<BrowserLaunchContext> {
            #[cfg(target_os = "windows")]
            if let Some(webview) = &self.webview {
                return Some(BrowserLaunchContext {
                    environment: Some(webview.read(_cx).raw().environment()),
                });
            }
            None
        }

        /// Whether the page was clicked since the last check (drains the queue).
        ///
        /// The app polls this each tick and makes this pane the active one — the
        /// click never reaches gpui on its own, so without this the pane keeps its
        /// old highlight and keyboard actions (paste, restart, close) go to
        /// whichever pane was focused before.
        pub fn take_native_focus(&mut self, pane_active: bool) -> Option<NativeFocusDecision> {
            let mut events = Vec::new();
            while let Ok(next) = self.focus_events.try_recv() {
                events.push(next);
            }
            let decision = native_focus_decision(self.native_visible, pane_active, &events);
            if decision == Some(NativeFocusDecision::RestoreActive)
                && crate::ui_profile::is_enabled()
            {
                crate::ui_profile::event("browser", "discarded-unarmed-browser-gain");
            }
            decision
        }

        /// Hand the OS keyboard focus to the native webview, so typing (and paste)
        /// goes to the page rather than to the pane muxel last focused.
        pub fn focus_native(&self, cx: &App) {
            // Focusing a hidden child would pull focus out of whatever is actually
            // on screen (a modal, another tab).
            if !self.native_visible {
                return;
            }
            if let Some(wv) = &self.webview {
                let _ = wv.read(cx).raw().focus();
            }
        }

        pub fn address_focused(&self, window: &Window, cx: &App) -> bool {
            self.address.read(cx).focus_handle(cx).is_focused(window)
        }

        /// Reload the page the webview is *currently* on.
        ///
        /// This is wry's native reload, not a re-navigation to `self.url` and not a
        /// rebuild of the pane: the user may be several links deep, and refresh must
        /// reload where they actually are.
        pub fn reload(&mut self, cx: &mut Context<Self>) {
            if let Some(wv) = &self.webview {
                let _ = wv.read(cx).raw().reload();
            }
        }

        /// Step back / forward through the page's session history.
        ///
        /// Reload is the only navigation wry exposes natively, so history has to go
        /// through the page itself. That is a genuine limitation, not a shortcut:
        /// it means back/forward do nothing on a document where scripts can't run
        /// (a webview that failed to load, `about:blank`), where a native
        /// `canGoBack` would at least have told us the button was pointless. The
        /// buttons therefore stay enabled and simply no-op at the ends of history,
        /// which is also what they did before.
        fn history_go(&self, delta: i32, cx: &App) {
            if let Some(wv) = &self.webview {
                let _ = wv
                    .read(cx)
                    .raw()
                    .evaluate_script(&format!("history.go({delta});"));
            }
        }

        pub fn back(&mut self, cx: &mut Context<Self>) {
            self.history_go(-1, cx);
        }

        pub fn forward(&mut self, cx: &mut Context<Self>) {
            self.history_go(1, cx);
        }

        pub fn open_external(&self, cx: &mut Context<Self>) {
            cx.open_url(&self.url);
        }

        pub fn tab_title(&self) -> String {
            super::tab_label(&self.url)
        }

        /// The URL the pane is showing. A native webview child belongs to the
        /// window that created it, so moving a browser pane between windows
        /// (pop-out / re-dock) rebuilds it from this rather than reparenting.
        pub fn url(&self) -> &str {
            &self.url
        }

        /// Navigate the webview and remember the URL.
        pub fn navigate(&mut self, url: &str, cx: &mut Context<Self>) {
            if let Some(wv) = &self.webview {
                wv.update(cx, |wv, _| wv.load_url(url));
            }
            if self.url != url {
                self.pending_navigation_from =
                    Some(std::mem::replace(&mut self.url, url.to_string()));
            }
            cx.notify();
        }

        /// Pull the webview's current URL (the user may have clicked links);
        /// returns it when it changed since the last sync. Called from the app's
        /// tick so `Instance.browser_url` and the address bar stay fresh.
        pub fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<String> {
            let wv = self.webview.as_ref()?;
            let current = wv.read(cx).url().ok()?;
            // A newly created WebView2 reports its bootstrap document before
            // the requested navigation commits. Persisting that transient URL
            // loses the resource identity and lets a fast second click create
            // a duplicate pane.
            if current == "about:blank" && self.url != "about:blank" {
                return None;
            }
            // A requested navigation hasn't committed yet: the webview still
            // reports the page we're leaving, so don't sync it back.
            if self.pending_navigation_from.as_deref() == Some(current.as_str()) {
                return None;
            }
            let navigation_committed = self.pending_navigation_from.is_some();
            self.pending_navigation_from = None;
            // Deliberately no `current == self.url` early return: an unchanged URL
            // can still leave the address bar stale (a reused pane re-shown at the
            // same URL), and the address resync below is what fixes that.
            if current.is_empty() {
                return None;
            }
            let changed = current != self.url;
            if changed {
                self.url = current.clone();
            }
            // Don't stomp the address bar while the user is editing it.
            let address = self.address.read(cx);
            let address_stale = address.value() != current;
            let address_focused = address.focus_handle(cx).is_focused(window);
            if address_stale && !address_focused {
                let url = current.clone();
                self.address.update(cx, |s, cx| {
                    s.set_value(url, window, cx);
                    s.set_scroll_offset(point(px(0.), px(0.)), cx);
                });
                self.address
                    .read(cx)
                    .focus_handle(cx)
                    .dispatch_action(&MoveToStart, window, cx);
            }
            if changed || address_stale {
                cx.notify();
            }
            should_publish_url(changed, navigation_committed).then_some(current)
        }

        /// Show/hide the NATIVE child window. The app drives this every frame:
        /// hidden whenever an overlay covers the pane area or this tab isn't
        /// the active one (the native view otherwise floats above everything).
        pub fn set_native_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
            if self.native_visible == visible {
                return;
            }
            if visible {
                // A GotFocus queued while hidden belongs to the old visibility
                // generation. Drain it before showing so it cannot steal focus
                // after the user later selects this tab.
                while self.focus_events.try_recv().is_ok() {}
            }
            self.native_visible = visible;
            if let Some(wv) = &self.webview {
                wv.update(cx, |wv, _| if visible { wv.show() } else { wv.hide() });
            }
        }
    }

    impl Focusable for BrowserView {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for BrowserView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let toolbar = h_flex()
                .gap_1()
                .px_2()
                .py_1()
                .bg(cx.theme().secondary)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, window, cx| {
                        this.focus_handle.focus(window, cx);
                    }),
                )
                .child(
                    Button::new("browser-back")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .tooltip(t("Back"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.back(cx))),
                )
                .child(
                    Button::new("browser-forward")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowRight)
                        .tooltip(t("Forward"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.forward(cx))),
                )
                .child(
                    Button::new("browser-reload")
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path("icons/refresh.svg"))
                        .tooltip(t("Reload this page"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.reload(cx))),
                )
                .child(
                    Button::new("browser-open-external")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ExternalLink)
                        .tooltip(t("Open in system browser"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.open_external(cx))),
                )
                .child(
                    // gpui-wry returns focus to the native page for clicks outside
                    // its bounds. Claim this press after Input handles it so the
                    // window-level webview handler cannot steal focus back.
                    div()
                        .flex_1()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(&self.address)),
                );

            let content: AnyElement = match &self.webview {
                Some(wv) => div()
                    .flex_1()
                    .min_h_0()
                    .child(wv.clone())
                    .into_any_element(),
                // The native child is still being built; stay blank rather than
                // flash a failure the pane hasn't actually hit.
                None if !self.webview_failed => div().flex_1().into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(cx.theme().muted_foreground)
                    .child(t("The system webview failed to start."))
                    .into_any_element(),
            };

            v_flex()
                .size_full()
                .track_focus(&self.focus_handle)
                .child(toolbar)
                .child(content)
        }
    }
}

// ---------------------------------------------------------------------------
// Linux: placeholder pane (the real browser is a separate window).
// ---------------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use gpui_component::button::{Button, ButtonVariants as _};

    pub struct BrowserView {
        focus_handle: FocusHandle,
        url: String,
    }

    impl BrowserView {
        pub fn new(
            _instance_id: uuid::Uuid,
            url: String,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                url,
            }
        }

        pub fn tab_title(&self) -> String {
            super::tab_label(&self.url)
        }

        /// The URL the pane is showing (see the macOS/Windows impl).
        pub fn url(&self) -> &str {
            &self.url
        }

        pub fn sync(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<String> {
            None
        }

        pub fn set_native_visible(&mut self, _visible: bool, _cx: &mut Context<Self>) {}

        /// No embedded webview here, so clicks land on ordinary gpui elements and
        /// the pane's own `on_mouse_down` already focuses it (see the macOS/Windows
        /// impl for why that isn't true there).
        pub fn take_native_focus(&mut self, _pane_active: bool) -> Option<NativeFocusDecision> {
            None
        }

        /// No native child to focus.
        pub fn focus_native(&self, _cx: &App) {}

        pub fn address_focused(&self, _window: &Window, _cx: &App) -> bool {
            false
        }

        // No `reload` here: the placeholder has no toolbar and no page. Reload is
        // called only from the macOS/Windows toolbar, inside that impl.
    }

    impl Focusable for BrowserView {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for BrowserView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let url = self.url.clone();
            v_flex()
                .size_full()
                .track_focus(&self.focus_handle)
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(cx.theme().muted_foreground)
                .child(t("On Linux the built-in browser opens as its own window."))
                .child(
                    Button::new("browser-open-window")
                        .primary()
                        .label(t("Open in browser window"))
                        .on_click(cx.listener(move |_this, _e, _w, cx| {
                            if !super::spawn_browser_window(&url) {
                                cx.open_url(&url);
                            }
                        })),
                )
        }
    }
}

pub use imp::BrowserView;
