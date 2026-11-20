//! Native export downloads. A browser operation owns its staging directory until
//! completion or cancellation. Only a successful transfer can reach Save As.
use crate::connect::ServerOrigin;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    rc::Rc,
};
use tempfile::{NamedTempFile, TempDir};

fn suggested_name(origin: &ServerOrigin, candidate: &str) -> Option<String> {
    if !origin.same_origin(candidate) {
        return None;
    }
    let url = url::Url::parse(candidate).ok()?;
    let id = url.path().strip_prefix("/transfer/export/")?;
    if id.len() != 36
        || !id.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Some(format!("project-{id}.zip"))
}

const INVALID_EXPORT: &str =
    "The server did not return the requested project ZIP. Check your sign-in and retry the export.";

fn expected_response(origin: &ServerOrigin, job: &Export, uri: &str, mime: &str) -> bool {
    mime.eq_ignore_ascii_case("application/zip")
        && suggested_name(origin, uri).as_deref() == Some(job.name.as_str())
}

struct Export {
    directory: TempDir,
    name: String,
}

impl Export {
    fn new(name: String) -> io::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("ratio-export-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        Ok(Self {
            directory: builder.tempdir()?,
            name,
        })
    }

    fn archive(&self) -> PathBuf {
        self.directory.path().join("export.zip")
    }

    fn save(&self, target: &Path) -> io::Result<()> {
        // Hard linking keeps the download engine's metadata, including NTFS
        // Internet Zone information, and atomically fails on a name collision.
        let archive = self.archive();
        match fs::hard_link(&archive, target) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Err(err),
            Err(_) => {}
        }
        // The user can choose another volume or one without hard links. Stage
        // the copy on that volume first so a partial ZIP never becomes visible.
        let parent = target.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let copy = NamedTempFile::new_in(parent)?.into_temp_path();
        fs::copy(&archive, &copy)?;
        copy.persist_noclobber(target).map_err(|err| err.error)
    }
}

fn save_error(err: &io::Error) -> String {
    if err.kind() == io::ErrorKind::AlreadyExists {
        "A file already uses that name. Choose a new name for your export.".into()
    } else {
        format!("Could not save this export. Choose another location or cancel.\n\n{err}")
    }
}

pub struct Downloads {
    pending: Rc<RefCell<HashMap<PathBuf, Export>>>,
    origin: ServerOrigin,
}

pub fn configure<'a>(
    builder: wry::WebViewBuilder<'a>,
    origin: ServerOrigin,
) -> (wry::WebViewBuilder<'a>, Downloads) {
    let pending = Rc::new(RefCell::new(HashMap::new()));
    let requests = pending.clone();
    let allowed = origin.clone();
    // Replace Wry's built-in automatic-saves handler before creating the view.
    let builder = builder.with_download_started_handler(move |candidate, destination| {
        let Some(name) = suggested_name(&allowed, &candidate) else {
            return false;
        };
        let job = match Export::new(name) {
            Ok(job) => job,
            Err(err) => {
                let message = format!("Could not start the export.\n\n{err}");
                #[cfg(target_os = "linux")]
                gtk_error(None, message);
                #[cfg(target_os = "windows")]
                std::thread::spawn(move || windows_error(&message));
                return false;
            }
        };
        let archive = job.archive();
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::FileExt;
            *destination = PathBuf::from(gtk::gio::File::for_path(&archive).uri().as_str());
        }
        #[cfg(target_os = "windows")]
        {
            *destination = archive.clone();
        }
        requests.borrow_mut().insert(archive, job);
        true
    });
    (builder, Downloads { pending, origin })
}

#[cfg(target_os = "linux")]
impl Downloads {
    pub fn install(self, view: &wry::WebView) -> Result<(), String> {
        use gtk::prelude::*;
        use webkit2gtk::{DownloadExt, URIResponseExt, WebContextExt, WebViewExt};
        use wry::WebViewExtUnix;
        let native = view.webview();
        let context = native
            .context()
            .ok_or("The webview has no download context.")?;
        let owner = native.downgrade();
        context.connect_download_started(move |_, download| {
            let Some(view) = owner.upgrade() else {
                download.cancel();
                return;
            };
            if download.web_view().as_ref() != Some(&view) {
                download.cancel();
                return;
            }
            let parent = view
                .toplevel()
                .and_then(|v| v.downcast::<gtk::Window>().ok());
            let failed = self.pending.clone();
            let error_parent = parent.clone();
            download.connect_failed(move |download, err| {
                if gtk_destination(download)
                    .and_then(|path| failed.borrow_mut().remove(&path))
                    .is_some()
                {
                    gtk_error(
                        error_parent.clone(),
                        format!("The export download failed.\n\n{err}"),
                    );
                }
            });
            let completed = self.pending.clone();
            let allowed = self.origin.clone();
            download.connect_finished(move |download| {
                let Some(path) = gtk_destination(download) else {
                    return;
                };
                let Some(job) = completed.borrow_mut().remove(&path) else {
                    return;
                };
                if download.response().is_some_and(|response| {
                    (200..300).contains(&response.status_code())
                        && response
                            .uri()
                            .zip(response.mime_type())
                            .is_some_and(|(uri, mime)| {
                                expected_response(&allowed, &job, &uri, &mime)
                            })
                }) {
                    gtk_save_as(job, parent.clone());
                } else {
                    gtk_error(parent.clone(), INVALID_EXPORT.into());
                }
            });
        });
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn gtk_destination(download: &webkit2gtk::Download) -> Option<PathBuf> {
    use gtk::prelude::FileExt;
    use webkit2gtk::DownloadExt;
    let uri = download.destination()?;
    gtk::gio::File::for_uri(&uri).path()
}

#[cfg(target_os = "linux")]
fn gtk_error(parent: Option<gtk::Window>, message: String) {
    use gtk::prelude::*;
    gtk::glib::MainContext::default().spawn_local(async move {
        let dialog = gtk::MessageDialog::new(
            parent.as_ref(),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Error,
            gtk::ButtonsType::Close,
            &message,
        );
        dialog.set_title("Project export");
        dialog.run_future().await;
        dialog.close();
    });
}

#[cfg(target_os = "linux")]
fn gtk_save_as(job: Export, parent: Option<gtk::Window>) {
    use gtk::prelude::*;
    use std::sync::Arc;
    gtk::glib::MainContext::default().spawn_local(async move {
        let job = Arc::new(job);
        loop {
            let chooser = gtk::FileChooserNative::new(
                Some("Save project export"),
                parent.as_ref(),
                gtk::FileChooserAction::Save,
                Some("Save"),
                Some("Cancel"),
            );
            if let Some(directory) = dirs::download_dir().filter(|d| d.is_dir()) {
                chooser.set_current_folder(directory);
            }
            chooser.set_current_name(&job.name);
            if chooser.run_future().await != gtk::ResponseType::Accept {
                return;
            }
            let Some(path) = chooser.filename() else {
                return;
            };
            let save = job.clone();
            match gtk::gio::spawn_blocking(move || save.save(&path)).await {
                Ok(Ok(())) => return,
                Ok(Err(err)) => {
                    let dialog = gtk::MessageDialog::new(
                        parent.as_ref(),
                        gtk::DialogFlags::MODAL,
                        gtk::MessageType::Error,
                        gtk::ButtonsType::Close,
                        &save_error(&err),
                    );
                    dialog.run_future().await;
                    dialog.close();
                }
                Err(_) => {
                    gtk_error(parent, "An error interrupted the export.".into());
                    return;
                }
            }
        }
    });
}

#[cfg(target_os = "windows")]
impl Downloads {
    pub fn install(self, view: &wry::WebView) -> Result<(), String> {
        use webview2_com::{
            DownloadStartingEventHandler,
            Microsoft::Web::WebView2::Win32::{
                COREWEBVIEW2_DOWNLOAD_STATE, COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED,
                COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS, ICoreWebView2_4,
            },
            StateChangedEventHandler, take_pwstr,
        };
        use windows_core::{Interface, PWSTR};
        use wry::WebViewExtWindows;
        let engine: ICoreWebView2_4 = view.webview().cast().map_err(|e| format!("{e}"))?;
        // Wry has assigned the private staging path. Bind the directory to this
        // COM operation so concurrent downloads of the same URL stay independent.
        unsafe {
            engine
                .add_DownloadStarting(
                    &DownloadStartingEventHandler::create(Box::new(move |_, args| {
                        let Some(args) = args else { return Ok(()) };
                        let mut result = PWSTR::null();
                        args.ResultFilePath(&mut result)?;
                        let path = PathBuf::from(take_pwstr(result));
                        let Some(job) = self.pending.borrow_mut().remove(&path) else {
                            args.SetCancel(true)?;
                            return Ok(());
                        };
                        let operation = args.DownloadOperation()?;
                        let mut uri = PWSTR::null();
                        operation.Uri(&mut uri)?;
                        let uri = take_pwstr(uri);
                        let mut mime = PWSTR::null();
                        operation.MimeType(&mut mime)?;
                        if !expected_response(&self.origin, &job, &uri, &take_pwstr(mime)) {
                            args.SetCancel(true)?;
                            std::thread::spawn(|| windows_error(INVALID_EXPORT));
                            return Ok(());
                        }
                        let pending = RefCell::new(Some(job));
                        operation.add_StateChanged(
                            &StateChangedEventHandler::create(Box::new(move |op, _| {
                                let Some(op) = op else { return Ok(()) };
                                let mut state = COREWEBVIEW2_DOWNLOAD_STATE::default();
                                op.State(&mut state)?;
                                if state == COREWEBVIEW2_DOWNLOAD_STATE_IN_PROGRESS {
                                    return Ok(());
                                }
                                if let Some(job) = pending.take() {
                                    if state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
                                        // Opening a native modal from WebView2's STA callback
                                        // would re-enter the browser. The chooser has its own STA.
                                        std::thread::spawn(move || windows_save_as(job));
                                    } else {
                                        std::thread::spawn(|| {
                                            windows_error("The export download was interrupted.")
                                        });
                                    }
                                }
                                Ok(())
                            })),
                            &mut 0,
                        )?;
                        Ok(())
                    })),
                    &mut 0,
                )
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn windows_save_as(job: Export) {
    loop {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save project export")
            .add_filter("ZIP archive", &["zip"])
            .set_file_name(&job.name);
        if let Some(directory) = dirs::download_dir() {
            dialog = dialog.set_directory(directory);
        }
        let Some(path) = dialog.save_file() else {
            break;
        };
        match job.save(&path) {
            Ok(()) => break,
            Err(err) => windows_error(&save_error(&err)),
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_error(message: &str) {
    rfd::MessageDialog::new()
        .set_title("Project export")
        .set_level(rfd::MessageLevel::Error)
        .set_description(message)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn export_names_only_come_from_workspace_project_ids() {
        let origin = ServerOrigin::parse("https://work.example").unwrap();
        let id = "12e45678-e89b-42d3-a456-426614174000";
        let path = format!("/transfer/export/{id}");
        assert_eq!(
            suggested_name(&origin, &format!("{origin}{path}")),
            Some(format!("project-{id}.zip"))
        );
        for candidate in [
            format!("https://another.example{path}"),
            format!("{origin}/transfer/export/{id}.exe"),
            format!("{origin}/transfer/export/{id}/anything"),
            format!("{origin}/transfer/export/%2e%2e"),
            format!("blob:{origin}{path}"),
        ] {
            assert_eq!(suggested_name(&origin, &candidate), None);
        }
    }

    #[test]
    fn login_redirect_and_html_error_are_rejected_as_export_responses() {
        let origin = ServerOrigin::parse("https://work.example").unwrap();
        let id = "12e45678-e89b-42d3-a456-426614174000";
        let job = Export::new(format!("project-{id}.zip")).unwrap();
        let uri = format!("{origin}/transfer/export/{id}");
        assert!(expected_response(&origin, &job, &uri, "application/zip"));
        assert!(!expected_response(&origin, &job, &uri, "text/html"));
        assert!(!expected_response(
            &origin,
            &job,
            &format!("{origin}/login"),
            "text/html"
        ));
        let other_project = uri.replace("12e45678", "42e45678");
        assert!(!expected_response(
            &origin,
            &job,
            &other_project,
            "application/zip"
        ));
    }

    #[test]
    fn concurrent_exports_never_replace_the_first_completed_file() {
        let folder = tempfile::tempdir().unwrap();
        let target = folder.path().join("chosen.zip");
        let gate = Arc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let mut writers = Vec::new();
            for payload in ["zip A", "zip B"] {
                let export = Export::new("example.zip".into()).unwrap();
                fs::write(export.archive(), payload).unwrap();
                let gate = gate.clone();
                let path = target.clone();
                writers.push(scope.spawn(move || {
                    gate.wait();
                    (payload, export.save(&path))
                }));
            }
            writers
                .into_iter()
                .map(|x| x.join().unwrap())
                .collect::<Vec<_>>()
        });
        let saved = fs::read_to_string(&target).unwrap();
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        for (payload, result) in results {
            if payload == saved {
                result.unwrap();
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            }
        }
    }
}
