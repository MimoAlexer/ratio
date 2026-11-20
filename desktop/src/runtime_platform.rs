use crate::runtime::{Command, Event};
use muda::{
    Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
    accelerator::{Accelerator, Code, Modifiers},
};
use tao::{
    event_loop::{EventLoop, EventLoopBuilder, EventLoopProxy},
    window::Window,
};
use wry::{WebView, WebViewBuilder};

#[cfg(target_os = "linux")]
pub fn initialize() -> Result<(), String> {
    use std::{env, ffi::OsStr, os::unix::process::CommandExt, path::Path, process::Command};

    if env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_some_and(|value| value != "0") {
        return Err("Unset WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS and reopen Ratio to enable the WebKitGTK sandbox.".into());
    }

    // WebKitGTK 4.1 reads this switch while constructing each process pool.
    // A visible Wry view may have spawned a process before build_gtk returns.
    // Exec preserves arguments and PID and avoids mutating a live process environment.
    if env::var_os("WEBKIT_FORCE_SANDBOX").as_deref() != Some(OsStr::new("1")) {
        let executable = env::current_exe().map_err(|e| e.to_string())?;
        let error = Command::new(executable)
            .args(env::args_os().skip(1))
            .env("WEBKIT_FORCE_SANDBOX", "1")
            .exec();
        return Err(format!(
            "Could not start the sandboxed Ratio client. {error}"
        ));
    }

    // On these platforms WebKit can report sandboxing enabled while falling back
    // to a plain subprocess if its OS-level launcher cannot run.
    let result = if Path::new("/.flatpak-info").exists() {
        Command::new("flatpak-spawn")
            .args([
                "--sandbox",
                "--sandbox-expose-path-ro-try=/nonexistent-ratio",
                "true",
            ])
            .output()
    } else {
        if ["SNAP", "SNAP_NAME", "SNAP_REVISION"]
            .iter()
            .all(|name| env::var_os(name).is_some())
        {
            return Err("Open Ratio from the native desktop menu. WebKitGTK skips its process sandbox inside this Snap environment.".into());
        }
        Command::new("/usr/bin/bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--unshare-all",
                "/bin/true",
            ])
            .output()
    };
    let guidance = "Ratio needs a working WebKitGTK process sandbox. Check that your distribution’s bubblewrap package and user namespace support are available, or open the app from the native desktop.";
    let output = result.map_err(|e| format!("{guidance}\n\n{e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{guidance}\n\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub fn event_loop() -> EventLoop<Event> {
    let mut builder = EventLoopBuilder::with_user_event();
    #[cfg(target_os = "windows")]
    win::configure_events(&mut builder);
    builder.build()
}

pub fn menu(window: &Window, proxy: &EventLoopProxy<Event>) -> Result<Menu, String> {
    let item = |id, title, modifiers, code| {
        MenuItem::with_id(id, title, true, Some(Accelerator::new(modifiers, code)))
    };
    let navigation = Submenu::with_items(
        "&Navigate",
        true,
        &[
            &item("back", "&Back", Modifiers::ALT, Code::ArrowLeft),
            &item("forward", "&Forward", Modifiers::ALT, Code::ArrowRight),
            &item("reload", "&Reload", Modifiers::CONTROL, Code::KeyR),
        ],
    )
    .map_err(|e| e.to_string())?;
    let workspace = Submenu::with_items(
        "&Workspace",
        true,
        &[
            &item(
                "settings",
                "&Connection settings…",
                Modifiers::CONTROL,
                Code::Comma,
            ),
            &PredefinedMenuItem::separator(),
            &item("quit", "&Quit", Modifiers::CONTROL, Code::KeyQ),
        ],
    )
    .map_err(|e| e.to_string())?;
    let menu = Menu::with_items(&[&workspace, &navigation]).map_err(|e| e.to_string())?;

    #[cfg(target_os = "linux")]
    {
        use tao::platform::unix::WindowExtUnix;
        menu.init_for_gtk_window(window.gtk_window(), window.default_vbox())
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    win::attach_menu(&menu, window)?;

    let proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = match event.id.0.as_str() {
            "back" => Command::Back,
            "forward" => Command::Forward,
            "reload" => Command::Reload,
            "settings" => Command::Settings,
            "quit" => Command::Quit,
            _ => return,
        };
        let _ = proxy.send_event(Event::Command(command));
    }));
    Ok(menu)
}

#[cfg(target_os = "linux")]
pub fn mount(builder: WebViewBuilder<'_>, window: &Window) -> Result<WebView, String> {
    use tao::platform::unix::WindowExtUnix;
    use webkit2gtk::{WebContextExt, WebViewExt};
    use wry::{WebViewBuilderExtUnix, WebViewExtUnix};
    let content = window
        .default_vbox()
        .ok_or("The GTK window has no content area.")?;
    let view = builder.build_gtk(content).map_err(|e| e.to_string())?;
    if !view
        .webview()
        .context()
        .is_some_and(|ctx| ctx.is_sandbox_enabled())
    {
        return Err("WebKitGTK did not enable its process sandbox. Install the distribution’s WebKitGTK 4.1 and bubblewrap packages, then reopen Ratio.".into());
    }
    Ok(view)
}

#[cfg(target_os = "linux")]
pub fn confirm(window: &Window, title: &str, message: &str) -> bool {
    use gtk::prelude::*;
    use tao::platform::unix::WindowExtUnix;
    let dialog = gtk::MessageDialog::new(
        Some(window.gtk_window()),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question,
        gtk::ButtonsType::YesNo,
        message,
    );
    dialog.set_title(title);
    dialog.set_default_response(gtk::ResponseType::No);
    let accepted = dialog.run() == gtk::ResponseType::Yes;
    // The nested run has returned and no other code owns or uses this dialog.
    unsafe { dialog.destroy() };
    accepted
}

#[cfg(target_os = "linux")]
pub fn error(window: Option<&Window>, title: &str, message: &str) {
    use gtk::prelude::*;
    use tao::platform::unix::WindowExtUnix;
    if !gtk::is_initialized_main_thread() && gtk::init().is_err() {
        eprintln!("{title}: {message}");
        return;
    }
    let dialog = gtk::MessageDialog::new(
        window.map(WindowExtUnix::gtk_window),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Error,
        gtk::ButtonsType::Close,
        message,
    );
    dialog.set_title(title);
    dialog.run();
    // The nested run has returned and no other code owns or uses this dialog.
    unsafe { dialog.destroy() };
}

#[cfg(target_os = "linux")]
pub fn watch_failures(
    view: &WebView,
    epoch: u64,
    proxy: EventLoopProxy<Event>,
) -> Result<(), String> {
    use webkit2gtk::{NetworkError, PolicyError, SettingsExt, WebViewExt};
    use wry::WebViewExtUnix;

    let webkit = view.webview();
    webkit
        .settings()
        .ok_or("WebKitGTK did not supply browser settings.")?
        .set_user_agent_with_application_details(
            Some("RatioDesktop"),
            Some(env!("CARGO_PKG_VERSION")),
        );
    let network = proxy.clone();
    webkit.connect_load_failed(move |_, _, _, error| {
        // Navigating away or converting a navigation to a download also emits load-failed.
        if error.matches(NetworkError::Cancelled)
            || error.matches(PolicyError::FrameLoadInterruptedByPolicyChange)
        {
            return false;
        }
        let _ = network.send_event(Event::Failed(epoch));
        true
    });
    webkit.connect_load_failed_with_tls_errors(move |_, _, _, _| {
        let _ = proxy.send_event(Event::Failed(epoch));
        true
    });
    Ok(())
}

#[cfg(target_os = "windows")]
pub use win::{confirm, error, mount, watch_failures};

#[cfg(target_os = "windows")]
mod win {
    use super::*;
    use std::cell::Cell;
    use tao::platform::windows::{EventLoopBuilderExtWindows, WindowExtWindows};
    use webview2_com::{
        AcceleratorKeyPressedEventHandler, LaunchingExternalUriSchemeEventHandler,
        Microsoft::Web::WebView2::Win32::*, NavigationCompletedEventHandler,
        NavigationStartingEventHandler,
    };
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, WPARAM},
            UI::WindowsAndMessaging::{
                GA_ROOT, GetAncestor, HACCEL, MSG, TranslateAcceleratorW, WM_KEYDOWN, WM_SYSKEYDOWN,
            },
        },
        core::{BOOL, HSTRING, Interface, PWSTR},
    };
    use wry::WebViewExtWindows;

    // The app has one top-level window and a fixed menu for its whole event-loop lifetime.
    thread_local! {
        static ACCELERATORS: Cell<Option<(HWND, HACCEL)>> = const { Cell::new(None) };
    }

    pub(super) fn configure_events(builder: &mut EventLoopBuilder<Event>) {
        builder.with_msg_hook(|message| {
            // Tao supplies a live Win32 MSG for this callback.
            let message = unsafe { &*message.cast::<MSG>() };
            translate(message)
        });
    }

    fn translate(message: &MSG) -> bool {
        let Some((owner, table)) = ACCELERATORS.get() else {
            return false;
        };
        if !matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN)
            || message.hwnd.is_invalid()
            || unsafe { GetAncestor(message.hwnd, GA_ROOT) } != owner
        {
            return false;
        }
        unsafe { TranslateAcceleratorW(owner, table, message) != 0 }
    }

    pub(super) fn attach_menu(menu: &Menu, window: &Window) -> Result<(), String> {
        let handle = window.hwnd();
        // Tao owns the HWND and the caller retains this menu alongside its window.
        unsafe { menu.init_for_hwnd(handle) }.map_err(|e| e.to_string())?;
        ACCELERATORS.set(Some((HWND(handle as _), HACCEL(menu.haccel() as _))));
        Ok(())
    }

    pub fn mount(builder: WebViewBuilder<'_>, window: &Window) -> Result<WebView, String> {
        let view = builder.build(window).map_err(|e| {
            if matches!(&e, wry::Error::WebView2Error(webview2_com::Error::WindowsError(win))
                if win.code().0 as u32 == 0x80070002)
            {
                format!("Install the Microsoft Edge WebView2 Evergreen Runtime, then reopen Ratio.\n\nhttps://developer.microsoft.com/microsoft-edge/webview2/\n\n{e}")
            } else {
                e.to_string()
            }
        })?;
        restrict_navigation(&view).map_err(|e| {
            format!("Update the Microsoft Edge WebView2 Runtime and reopen Ratio. {e}")
        })?;
        let owner = HWND(window.hwnd() as _);
        // WebView2 processes keys in a separate process while the content has focus.
        // Route those keystrokes through the same menu table as native-window keystrokes.
        let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            unsafe {
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
                args.KeyEventKind(&mut kind)?;
                let message = match kind {
                    COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN => WM_KEYDOWN,
                    COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN => WM_SYSKEYDOWN,
                    _ => return Ok(()),
                };
                let mut key = 0;
                let mut data = 0;
                args.VirtualKey(&mut key)?;
                args.KeyEventLParam(&mut data)?;
                let key_message = MSG {
                    hwnd: owner,
                    message,
                    wParam: WPARAM(key as usize),
                    lParam: LPARAM(data as isize),
                    ..Default::default()
                };
                if translate(&key_message) {
                    args.SetHandled(true)?;
                }
            }
            Ok(())
        }));
        unsafe {
            view.controller()
                .add_AcceleratorKeyPressed(&handler, &mut 0)
        }
        .map_err(|e| e.to_string())?;
        Ok(view)
    }

    pub fn confirm(window: &Window, title: &str, message: &str) -> bool {
        use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
        MessageDialog::new()
            .set_parent(window)
            .set_title(title)
            .set_description(message)
            .set_level(MessageLevel::Info)
            .set_buttons(MessageButtons::YesNo)
            .show()
            == MessageDialogResult::Yes
    }

    pub fn error(window: Option<&Window>, title: &str, message: &str) {
        use rfd::{MessageDialog, MessageLevel};
        let mut dialog = MessageDialog::new()
            .set_title(title)
            .set_description(message)
            .set_level(MessageLevel::Error);
        if let Some(window) = window {
            dialog = dialog.set_parent(window);
        }
        dialog.show();
    }

    fn restrict_navigation(view: &WebView) -> windows::core::Result<()> {
        let webview = view.webview();
        // WebView2 handles child frames and external OS protocols outside the
        // top-level navigation callback. Ratio uses a single document.
        let frame = NavigationStartingEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                unsafe { args.SetCancel(true)? };
            }
            Ok(())
        }));
        let external = LaunchingExternalUriSchemeEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                unsafe { args.SetCancel(true)? };
            }
            Ok(())
        }));
        unsafe {
            webview.add_FrameNavigationStarting(&frame, &mut 0)?;
            webview
                .cast::<ICoreWebView2_18>()?
                .add_LaunchingExternalUriScheme(&external, &mut 0)
        }
    }

    pub fn watch_failures(
        view: &WebView,
        epoch: u64,
        proxy: EventLoopProxy<Event>,
    ) -> Result<(), String> {
        let webview = view.webview();
        set_user_agent(&webview).map_err(|e| e.to_string())?;
        let handler = NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            unsafe {
                let mut success = BOOL::default();
                args.IsSuccess(&mut success)?;
                if success.as_bool() {
                    return Ok(());
                }
                let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                args.WebErrorStatus(&mut status)?;
                // A download or intentionally blocked outgoing link cancels its navigation.
                if status == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED {
                    return Ok(());
                }
                if let Ok(http) = args.cast::<ICoreWebView2NavigationCompletedEventArgs2>() {
                    let mut code = 0;
                    http.HttpStatusCode(&mut code)?;
                    // Let the application render its own error pages, including invalid forms.
                    if code >= 400 {
                        return Ok(());
                    }
                }
            }
            let _ = proxy.send_event(Event::Failed(epoch));
            Ok(())
        }));
        unsafe { webview.add_NavigationCompleted(&handler, &mut 0) }.map_err(|e| e.to_string())
    }

    fn set_user_agent(webview: &ICoreWebView2) -> windows::core::Result<()> {
        unsafe {
            let settings: ICoreWebView2Settings2 = webview.Settings()?.cast()?;
            let mut original = PWSTR::null();
            settings.UserAgent(&mut original)?;
            let agent = format!(
                "{} RatioDesktop/{}",
                webview2_com::take_pwstr(original),
                env!("CARGO_PKG_VERSION")
            );
            settings.SetUserAgent(&HSTRING::from(agent))
        }
    }
}
