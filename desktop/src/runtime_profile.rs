use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Keep WebKitGTK and WebView2 at their original absolute storage path on upgrade.
/// An established Ratio profile takes precedence if both applications have been used.
pub(crate) fn default_profile(data_root: &Path) -> Result<PathBuf, String> {
    let ratio = data_root.join("Ratio");
    for candidate in [&ratio, &data_root.join("Working Set")] {
        match fs::metadata(candidate) {
            Ok(metadata) if metadata.is_dir() => return Ok(candidate.clone()),
            Ok(_) => {
                return Err(format!(
                    "The desktop profile at {} must be a directory.",
                    candidate.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Could not inspect the desktop profile at {}: {error}",
                    candidate.display()
                ));
            }
        }
    }
    Ok(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::{ServerOrigin, load_origin, save_origin};
    use std::io::Read;
    use tempfile::tempdir;

    #[test]
    fn fresh_install_uses_ratio_without_creating_a_legacy_directory() {
        let data = tempdir().unwrap();
        let selected = default_profile(data.path()).unwrap();
        assert_eq!(selected, data.path().join("Ratio"));
        fs::create_dir(&selected).unwrap();
        assert_eq!(default_profile(data.path()).unwrap(), selected);
        assert!(!data.path().join("Working Set").exists());
    }

    #[test]
    fn upgrade_reopens_the_live_workspace_and_its_original_cookie_store() {
        let data = tempdir().unwrap();
        let old = data.path().join("Working Set");
        let origin = ServerOrigin::parse("https://workspace.example").unwrap();
        save_origin(&old, Some(&origin)).unwrap();
        let webview = old.join("profiles").join(origin.profile_key());
        fs::create_dir_all(&webview).unwrap();
        let cookies = webview.join("cookies");
        fs::write(&cookies, b"existing browser state").unwrap();
        let mut live_handle = fs::File::open(&cookies).unwrap();

        let selected = default_profile(data.path()).unwrap();
        assert_eq!(selected, old);
        let saved = load_origin(&selected).unwrap().unwrap();
        assert_eq!(saved, origin);
        assert_eq!(
            fs::read(
                selected
                    .join("profiles")
                    .join(saved.profile_key())
                    .join("cookies")
            )
            .unwrap(),
            b"existing browser state"
        );
        let mut original = String::new();
        live_handle.read_to_string(&mut original).unwrap();
        assert_eq!(original, "existing browser state");
        assert!(!data.path().join("Ratio").exists());
    }

    #[test]
    fn an_established_ratio_profile_stays_selected_when_both_exist() {
        let data = tempdir().unwrap();
        let ratio = data.path().join("Ratio");
        let old = data.path().join("Working Set");
        let primary = ServerOrigin::parse("https://current.example").unwrap();
        let legacy = ServerOrigin::parse("https://old.example").unwrap();
        save_origin(&ratio, Some(&primary)).unwrap();
        save_origin(&old, Some(&legacy)).unwrap();

        assert_eq!(default_profile(data.path()).unwrap(), ratio);
        assert_eq!(load_origin(&ratio).unwrap(), Some(primary));
        assert_eq!(load_origin(&old).unwrap(), Some(legacy));
    }
}
