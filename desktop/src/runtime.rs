use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

use serde::Deserialize;
use tao::{
    dpi::LogicalSize,
    event::{Event as TaoEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopProxy},
    window::{Icon, Window, WindowBuilder},
};
use url::Url;
use wry::{
    NewWindowResponse, PermissionResponse, WebContext, WebView, WebViewBuilder, http::Response,
};

use crate::{
    Options,
    connect::{ServerOrigin, launcher_html, load_origin, save_origin},
    runtime_download, runtime_platform,
};

const LAUNCHER: &str = "ratio://localhost/";
#[cfg(target_os = "windows")]
const LAUNCHER_DOCUMENT: &str = "https://ratio.localhost/";
#[cfg(not(target_os = "windows"))]
const LAUNCHER_DOCUMENT: &str = LAUNCHER;

#[derive(Debug)]
pub(crate) enum Command {
    Back,
    Forward,
    Reload,
    Settings,
    Quit,
}

#[derive(Debug)]
pub(crate) enum Event {
    Command(Command),
    Launcher(u64, String),
    Open(u64, Url),
    Navigate(u64, String),
    Failed(u64),
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum LauncherCommand {
    Connect { origin: String },
    Forget,
}

struct Page {
    view: WebView,
    // The view must be destroyed before its native context.
    _context: WebContext,
    remote: bool,
}

struct Desktop {
    page: Option<Page>,
    _menu: muda::Menu,
    window: Window,
    proxy: EventLoopProxy<Event>,
    profile: PathBuf,
    origin: Option<ServerOrigin>,
    epoch: u64,
    pending_external: Rc<Cell<bool>>,
}

fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|error| format!("Could not create the desktop profile: {error}"))
}

pub(crate) fn run(options: Options) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    runtime_platform::initialize()?;
    let profile = match options.profile {
        Some(path) => std::path::absolute(path).map_err(|e| e.to_string())?,
        None => {
            let data_root =
                dirs::data_local_dir().ok_or("No user profile directory is available")?;
            crate::runtime_profile::default_profile(&data_root)?
        }
    };
    private_dir(&profile)?;

    let (origin, initial_message) = match options.origin {
        Some(origin) => {
            save_origin(&profile, Some(&origin))?;
            (Some(origin), None)
        }
        None => match load_origin(&profile) {
            Ok(origin) => (origin, None),
            Err(reason) => (None, Some(reason)),
        },
    };

    let event_loop = runtime_platform::event_loop();
    let proxy = event_loop.create_proxy();
    let icon = Icon::from_rgba(include_bytes!("../assets/icon.rgba").to_vec(), 32, 32)
        .map_err(|error| error.to_string())?;
    let window = WindowBuilder::new()
        .with_title("Ratio")
        .with_window_icon(Some(icon))
        .with_inner_size(LogicalSize::new(1240, 820))
        .with_min_inner_size(LogicalSize::new(520, 440))
        .build(&event_loop)
        .map_err(|error| error.to_string())?;
    let menu = runtime_platform::menu(&window, &proxy)?;
    let mut app = Desktop {
        page: None,
        window,
        _menu: menu,
        proxy,
        profile,
        origin,
        epoch: 0,
        pending_external: Rc::new(Cell::new(false)),
    };
    if app.origin.is_some() {
        if let Err(reason) = app.workspace() {
            app.launcher(Some(&reason))?;
        }
    } else {
        app.launcher(initial_message.as_deref())?;
    }

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            TaoEvent::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | TaoEvent::UserEvent(Event::Command(Command::Quit)) => {
                app.page.take();
                *flow = ControlFlow::Exit;
            }
            TaoEvent::UserEvent(event) => {
                if let Err(error) = app.handle(event)
                    && let Err(fatal) = app.launcher(Some(&error))
                {
                    runtime_platform::error(
                        Some(&app.window),
                        "Ratio could not open connection settings",
                        &fatal,
                    );
                    *flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}

impl Desktop {
    fn replace_page(&mut self) -> u64 {
        self.epoch += 1;
        self.pending_external.set(false);
        self.page.take();
        self.epoch
    }

    fn launcher(&mut self, reason: Option<&str>) -> Result<(), String> {
        let epoch = self.replace_page();
        let page = launcher_html(self.origin.as_ref(), reason, None);
        let directory = self.profile.join("launcher");
        private_dir(&directory)?;
        let mut context = WebContext::new(Some(directory));
        let proxy = self.proxy.clone();
        let builder = WebViewBuilder::new_with_web_context(&mut context)
            .with_custom_protocol("ratio".into(), move |_, request| {
                if request.uri() != LAUNCHER || request.method() != "GET" {
                    return Response::builder().status(404).body(Vec::new().into()).expect("valid static response");
                }
                Response::builder()
                    .header("Content-Type", "text/html; charset=utf-8")
                    .header("Cache-Control", "no-store")
                    .header("Content-Security-Policy", "default-src 'none'; frame-src 'none'; frame-ancestors 'none'; form-action 'none'; base-uri 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:")
                    .body(page.as_bytes().to_vec().into()).expect("valid static response")
            })
            .with_navigation_handler(|url| url == LAUNCHER_DOCUMENT)
            .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
            .with_download_started_handler(|_, _| false)
            .with_permission_handler(|_| PermissionResponse::Deny)
            .with_devtools(false)
            .with_ipc_handler(move |request| {
                if request.uri() == LAUNCHER_DOCUMENT && request.body().len() <= 8192 {
                    let _ = proxy.send_event(Event::Launcher(epoch, request.into_body()));
                }
            });
        #[cfg(target_os = "windows")]
        let builder = {
            use wry::WebViewBuilderExtWindows;
            builder
                .with_https_scheme(true)
                .with_default_context_menus(false)
                .with_url(LAUNCHER)
        };
        let view = runtime_platform::mount(builder, &self.window)?;
        #[cfg(target_os = "linux")]
        view.load_url(LAUNCHER).map_err(|e| e.to_string())?;
        self.window.set_title("Ratio — Connection");
        self.page = Some(Page {
            view,
            _context: context,
            remote: false,
        });
        Ok(())
    }

    fn workspace(&mut self) -> Result<(), String> {
        let origin = self
            .origin
            .clone()
            .ok_or("Choose a Ratio server to connect.")?;
        let epoch = self.replace_page();
        let directory = self.profile.join("profiles").join(origin.profile_key());
        private_dir(&directory)?;
        let mut context = WebContext::new(Some(directory));
        let proxy = self.proxy.clone();
        let pending = self.pending_external.clone();
        let ask_external = move |url: Url| {
            if !pending.replace(true) {
                let _ = proxy.send_event(Event::Open(epoch, url));
            }
        };

        let navigation = origin.clone();
        let ask_on_navigation = ask_external.clone();
        let popup_origin = origin.clone();
        let popup_proxy = self.proxy.clone();
        // This view has no desktop protocol or IPC message handler.
        let builder = WebViewBuilder::new_with_web_context(&mut context)
            .with_devtools(false)
            .with_back_forward_navigation_gestures(true)
            .with_permission_handler(|_| PermissionResponse::Deny)
            .with_navigation_handler(move |candidate| {
                if navigation.same_origin(&candidate) {
                    return true;
                }
                if let Some(url) = external_url(&candidate) {
                    ask_on_navigation(url);
                }
                false
            })
            .with_new_window_req_handler(move |candidate, _| {
                if popup_origin.same_origin(&candidate) {
                    if let Ok(url) = Url::parse(&candidate) {
                        if workspace_browser_action(&url).is_some() {
                            ask_external(url);
                        } else {
                            let _ = popup_proxy.send_event(Event::Navigate(epoch, candidate));
                        }
                    }
                } else if let Some(url) = external_url(&candidate) {
                    ask_external(url);
                }
                NewWindowResponse::Deny
            });
        let (builder, downloads) = runtime_download::configure(builder, origin.clone());
        let view = runtime_platform::mount(builder, &self.window)?;
        runtime_platform::watch_failures(&view, epoch, self.proxy.clone())?;
        downloads.install(&view)?;
        self.window
            .set_title(&format!("Ratio — {}", origin.as_str()));
        view.load_url(&format!("{}/desktop/start", origin.as_str()))
            .map_err(|e| e.to_string())?;
        self.page = Some(Page {
            view,
            _context: context,
            remote: true,
        });
        Ok(())
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Command(command) => self.command(command),
            Event::Launcher(epoch, body) if epoch == self.epoch => self.launcher_command(&body),
            Event::Failed(epoch) if epoch == self.epoch => {
                self.launcher(Some("Ratio could not reach this server. Check the address and your network, then connect again."))
            }
            Event::Open(epoch, url) if epoch == self.epoch => {
                self.open_external(url);
                Ok(())
            }
            Event::Navigate(epoch, candidate) if epoch == self.epoch => {
                let trusted = self.origin.as_ref().is_some_and(|origin| origin.same_origin(&candidate));
                if let Some(page) = self.page.as_ref().filter(|page| page.remote && trusted) {
                    page.view.load_url(&candidate).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn command(&mut self, command: Command) -> Result<(), String> {
        if matches!(command, Command::Settings) {
            return self.launcher(None);
        }
        let Some(page) = self.page.as_ref() else {
            return Ok(());
        };
        if !page.remote {
            return if matches!(command, Command::Reload) {
                self.launcher(None)
            } else {
                Ok(())
            };
        }
        match command {
            Command::Back => page.view.go_back(),
            Command::Forward => page.view.go_forward(),
            Command::Reload => page.view.reload(),
            _ => return Ok(()),
        }
        .map_err(|e| e.to_string())
    }

    fn launcher_command(&mut self, body: &str) -> Result<(), String> {
        let Some(page) = self.page.as_ref().filter(|page| !page.remote) else {
            return Ok(());
        };
        let Ok(command) = serde_json::from_str::<LauncherCommand>(body) else {
            return Ok(());
        };
        match command {
            LauncherCommand::Connect { origin } => {
                let origin = match ServerOrigin::parse(&origin) {
                    Ok(origin) => origin,
                    Err(reason) => {
                        let message = serde_json::to_string(&reason).map_err(|e| e.to_string())?;
                        return page
                            .view
                            .evaluate_script(&format!("showInvalidAddress({message})"))
                            .map_err(|e| e.to_string());
                    }
                };
                save_origin(&self.profile, Some(&origin))?;
                self.origin = Some(origin);
                self.workspace()
            }
            LauncherCommand::Forget => {
                save_origin(&self.profile, None)?;
                self.origin = None;
                self.launcher(None)
            }
        }
    }

    fn open_external(&self, url: Url) {
        let destination = url.origin().ascii_serialization();
        let action = self
            .origin
            .as_ref()
            .filter(|origin| origin.same_origin(url.as_str()))
            .and_then(|_| workspace_browser_action(&url));
        let description = match action {
            Some(BrowserAction::Pair) => format!(
                "Open {destination} in your default browser to approve this desktop sign-in?"
            ),
            Some(BrowserAction::GitHub) => format!(
                "Open {destination} in your default browser to connect GitHub to your Ratio account?"
            ),
            None => format!(
                "Open this link from your workspace in your default browser?\n\n{destination}{}",
                url.path()
            ),
        };
        let approved = runtime_platform::confirm(&self.window, "Open in browser", &description);
        self.pending_external.set(false);
        if approved && let Err(error) = open::that_detached(url.as_str()) {
            runtime_platform::error(
                Some(&self.window),
                "Could not open your browser",
                &error.to_string(),
            );
        }
    }
}

fn external_url(candidate: &str) -> Option<Url> {
    if candidate.len() > 8192 || candidate.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(candidate).ok()?;
    (url.scheme() == "https"
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

#[derive(Debug, Eq, PartialEq)]
enum BrowserAction {
    Pair,
    GitHub,
}

// These two browser routes carry a public pairing code or account identifier.
// Keep all other workspace routes inside the signed-in desktop session.
fn workspace_browser_action(url: &Url) -> Option<BrowserAction> {
    if url.fragment().is_some() {
        return None;
    }
    let mut query = url.query_pairs();
    let (name, value) = query.next()?;
    if query.next().is_some() {
        return None;
    }
    match (url.path(), name.as_ref()) {
        ("/desktop/authorize", "code") => {
            let valid = value.len() == 9
                && value.bytes().enumerate().all(|(index, byte)| {
                    if index == 4 {
                        byte == b'-'
                    } else {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit()
                    }
                });
            valid.then_some(BrowserAction::Pair)
        }
        ("/integrations/github/browser", "owner") => {
            let valid = value.len() == 36
                && value.bytes().enumerate().all(|(index, byte)| {
                    if matches!(index, 8 | 13 | 18 | 23) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                    }
                });
            valid.then_some(BrowserAction::GitHub)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_handoff_accepts_only_https_without_credentials() {
        assert!(
            external_url("https://github.com/login/oauth/authorize?client_id=example").is_some()
        );
        for candidate in [
            "file:///tmp/report",
            "http://example.org",
            "mailto:a@example.org",
            "https://person:secret@example.org",
            "https://example.org\n/x",
        ] {
            assert!(external_url(candidate).is_none());
        }
    }

    #[test]
    fn desktop_approval_is_the_exact_public_code_route() {
        let valid = "http://127.0.0.1:4000/desktop/authorize?code=123A-BC45";
        assert_eq!(
            workspace_browser_action(&Url::parse(valid).unwrap()),
            Some(BrowserAction::Pair)
        );
        for invalid in [
            format!("{valid}&next=x"),
            format!("{valid}#token"),
            valid.replace("123A-BC45", "secret"),
            valid.replace("/authorize", "/other"),
        ] {
            assert_eq!(
                workspace_browser_action(&Url::parse(&invalid).unwrap()),
                None
            );
        }
    }
    #[test]
    fn github_browser_route_accepts_only_a_single_canonical_account_id() {
        let valid = "https://example.test/integrations/github/browser?owner=543cf04c-77a5-477b-8766-80de86679eed";
        assert_eq!(
            workspace_browser_action(&Url::parse(valid).unwrap()),
            Some(BrowserAction::GitHub)
        );
        for invalid in [
            format!("{valid}&next=https://elsewhere.test"),
            format!("{valid}#other"),
            valid.replace("/browser", "/connect"),
            valid.replace("owner=", "token="),
            valid.replace("543c", "543C"),
            valid.replace("77a5-", "77a5x"),
        ] {
            assert_eq!(
                workspace_browser_action(&Url::parse(&invalid).unwrap()),
                None
            );
        }
    }
}
