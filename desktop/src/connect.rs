use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::path::Path;
use tempfile::NamedTempFile;
use url::{Host, Url};

const ORIGIN_FILE: &str = "workspace-origin";
const MAX_ORIGIN_BYTES: usize = 4096;
// The native WebViews can silently reject these HTTP(S) ports before loading a page.
// https://fetch.spec.whatwg.org/#port-blocking
const FETCH_BLOCKED_PORTS: &[u16] = &[
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667,
    6668, 6669, 6679, 6697, 10080,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerOrigin(String);

impl ServerOrigin {
    /// Accept a server's origin, never a deep link, access token or sign-in URL.
    pub fn parse(input: &str) -> Result<Self, String> {
        let address = input.trim();
        if address.len() > MAX_ORIGIN_BYTES {
            return Err("The workspace address is too long.".into());
        }
        let (url, authority, suffix) = parse_absolute_url(address)
            .ok_or("Enter a full workspace address, such as https://workspace.example.com.")?;
        if !matches!(suffix, "" | "/") {
            return Err(
                "Use the server address by itself. Remove any page path, query or fragment.".into(),
            );
        }
        if let Some(port) = url.port().filter(|port| FETCH_BLOCKED_PORTS.contains(port)) {
            return Err(format!(
                "The browser blocks port {port}. Enter a workspace address on an allowed web port."
            ));
        }
        match url.scheme() {
            "https" => {}
            "http" if is_explicit_loopback(&url, authority) => {}
            _ => return Err("Use HTTPS for a deployed workspace. HTTP is available for a server on this computer.".into()),
        }
        let origin = url.origin().ascii_serialization();
        if origin.len() > MAX_ORIGIN_BYTES {
            return Err("The workspace address is too long.".into());
        }
        Ok(Self(origin))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Navigation within a workspace may include a route, query or fragment.
    pub fn same_origin(&self, candidate: &str) -> bool {
        parse_absolute_url(candidate)
            .is_some_and(|(url, _, _)| url.origin().ascii_serialization() == self.0)
    }

    /// Stable, filesystem-safe directory name that keeps WebView sessions separate.
    pub fn profile_key(&self) -> String {
        format!("{:x}", Sha256::digest(self.0.as_bytes()))
    }
}

impl fmt::Display for ServerOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn parse_absolute_url(address: &str) -> Option<(Url, &str, &str)> {
    // The browser URL parser also repairs missing authorities and strips tabs and
    // newlines. Only accept the explicit address the user sees in the field.
    if address.contains('\\') || address.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let (scheme, after_scheme) = address.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let (authority, suffix) = after_scheme.split_at(end);
    if authority.is_empty()
        || authority.contains(['@', '%', '"', '\'', '<', '>'])
        || authority.ends_with(':')
    {
        return None;
    }
    let url = Url::parse(address).ok()?;
    url.host()?;
    Some((url, authority, suffix))
}

fn is_explicit_loopback(url: &Url, authority: &str) -> bool {
    match url.host() {
        Some(Host::Domain("localhost")) => authority
            .split(':')
            .next()
            .is_some_and(|raw| raw.eq_ignore_ascii_case("localhost")),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Ipv4(address)) if address.is_loopback() => {
            // Reject legacy octal, hexadecimal and shortened numeric hostnames.
            authority
                .split(':')
                .next()
                .and_then(|raw| raw.parse::<Ipv4Addr>().ok())
                == Some(address)
        }
        _ => false,
    }
}

pub fn load_origin(config_dir: &Path) -> Result<Option<ServerOrigin>, String> {
    let file = match File::open(config_dir.join(ORIGIN_FILE)) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "Could not open the saved workspace address ({err})."
            ));
        }
    };
    let mut address = String::new();
    file.take((MAX_ORIGIN_BYTES + 2) as u64)
        .read_to_string(&mut address)
        .map_err(|err| format!("Could not read the saved workspace address ({err})."))?;
    if address.len() > MAX_ORIGIN_BYTES + 1 {
        return Err("The saved workspace address is too long. Enter it again.".into());
    }
    ServerOrigin::parse(&address).map(Some)
}

pub fn save_origin(config_dir: &Path, value: Option<&ServerOrigin>) -> Result<(), String> {
    let target = config_dir.join(ORIGIN_FILE);
    let Some(origin) = value else {
        return match fs::remove_file(target) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!(
                "Could not forget the saved workspace address ({err})."
            )),
        };
    };
    let replace = || -> io::Result<()> {
        fs::create_dir_all(config_dir)?;
        let mut pending = NamedTempFile::new_in(config_dir)?;
        writeln!(pending, "{origin}")?;
        pending.as_file().sync_all()?;
        // persist atomically replaces an existing file on both Unix and Windows.
        pending.persist(target)?;
        Ok(())
    };
    replace()
        .map_err(|err| format!("Could not save the workspace address on this computer ({err})."))
}

fn html_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

pub fn launcher_html(
    saved: Option<&ServerOrigin>,
    message: Option<&str>,
    local_demo: Option<&ServerOrigin>,
) -> String {
    let origin = html_text(saved.map_or("", ServerOrigin::as_str));
    let demo_origin = html_text(local_demo.map_or("", ServerOrigin::as_str));
    let status = html_text(message.unwrap_or_default());
    let template = include_str!("../assets/connect.html");
    let mut result = String::with_capacity(template.len() + status.len());

    // Substitute in one pass so text from status messages is never reinterpreted.
    let mut parts = template.split("{{");
    result.push_str(parts.next().unwrap_or_default());
    for part in parts {
        let (key, rest) = part
            .split_once("}}")
            .expect("closed connection template token");
        result.push_str(match key {
            "MARK" => include_str!("../assets/mark.svg"),
            "ORIGIN" => &origin,
            "DEMO_ORIGIN" => &demo_origin,
            "MESSAGE" => &status,
            "SAVED_HIDDEN" => {
                if saved.is_none() {
                    "hidden"
                } else {
                    ""
                }
            }
            "DEMO_HIDDEN" => {
                if local_demo.is_none() {
                    "hidden"
                } else {
                    ""
                }
            }
            _ => panic!("unknown connection template token: {key}"),
        });
        result.push_str(rest);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn canonical_addresses_share_a_profile_across_restarts() {
        let first = ServerOrigin::parse("  HTTPS://Workspace.Example:443/ ").unwrap();
        let second = ServerOrigin::parse("https://workspace.example").unwrap();
        assert_eq!(first.as_str(), "https://workspace.example");
        assert_eq!(first.profile_key(), second.profile_key());
        // Existing installations use this directory name for this origin's cookies.
        assert_eq!(
            first.profile_key(),
            "8425d238f616aa2a3b2d9c49515b913ec9a18a5c72b11c5a2565cbb510eb2424"
        );
        let other_port = ServerOrigin::parse("https://workspace.example:444").unwrap();
        assert_ne!(first.profile_key(), other_port.profile_key());
        assert_eq!(
            ServerOrigin::parse("https://bücher.example")
                .unwrap()
                .as_str(),
            "https://xn--bcher-kva.example"
        );
    }

    #[test]
    fn plaintext_is_limited_to_explicit_loopback_addresses() {
        for accepted in [
            "http://localhost:3000",
            "http://127.42.1.2",
            "http://[::1]:8080",
            "https://tracker.internal",
        ] {
            assert!(ServerOrigin::parse(accepted).is_ok(), "{accepted}");
        }
        for rejected in [
            "http://example.test",
            "http://192.168.0.5",
            "http://[::]",
            "http://0x7f000001",
            "http://127.1",
            "http://localhost.example",
            "http://ｌｏｃａｌｈｏｓｔ",
        ] {
            assert!(ServerOrigin::parse(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn browsers_block_reserved_ports_before_a_workspace_can_load() {
        for port in [1, 9, 0, 22, 4190, 6668, 10080] {
            for host in ["http://127.0.0.1", "https://work.example"] {
                let error = ServerOrigin::parse(&format!("{host}:{port}")).unwrap_err();
                assert!(error.contains(&format!("port {port}")), "{error}");
            }
        }
        for port in [80, 443, 3000, 4178, 6664, 6670, 10081] {
            assert!(ServerOrigin::parse(&format!("http://127.0.0.1:{port}")).is_ok());
        }
    }

    #[test]
    fn only_unambiguous_origins_are_saved() {
        for bad in [
            "",
            "file:///tmp",
            "https:example.test",
            "https:work.example/secret?key=https://fake.test",
            "//example.test",
            "https://user:pw@host",
            "https://@host",
            "https://host/path",
            "https://host/.",
            "https://host//",
            "https://host?token=foo",
            "https://host/#frag",
            "https://example.test\\@host",
            "https://foo\nbar",
            "https://",
            "https:////example.test",
            "https://host:0",
            "https://%65xample.test",
            "https://example.test:",
        ] {
            assert!(ServerOrigin::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn saved_addresses_fit_the_read_limit_after_idna_conversion() {
        let unicode_name = format!("https://{}example", "é.".repeat(650));
        assert!(ServerOrigin::parse(&unicode_name).is_err());
    }

    #[test]
    fn workspace_navigation_checks_scheme_host_and_effective_port() {
        let server = ServerOrigin::parse("https://work.example").unwrap();
        for same in [
            "https://work.example",
            "HTTPS://WORK.EXAMPLE:443/projects?view=one#item",
            "https://work.example/file%20one",
        ] {
            assert!(server.same_origin(same), "{same}");
        }
        for other in [
            "http://work.example",
            "https://work.example:8443",
            "https://work.example.other",
            "https://work.example@other",
            "https://user@work.example",
            "https://@work.example",
            "about:blank",
            "blob:https://work.example/uuid",
            "https:////work.example",
            "https:work.example/x?next=https://other.example",
            "https://other\\@work.example",
        ] {
            assert!(!server.same_origin(other), "{other}");
        }
    }

    #[test]
    fn replacement_and_forgetting_only_touch_the_selected_origin() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("config");
        assert_eq!(load_origin(&config).unwrap(), None);
        let first = ServerOrigin::parse("https://one.example").unwrap();
        let second = ServerOrigin::parse("https://two.example").unwrap();
        save_origin(&config, Some(&first)).unwrap();
        assert_eq!(load_origin(&config).unwrap(), Some(first));
        fs::write(config.join("other-setting"), "keep").unwrap();
        save_origin(&config, Some(&second)).unwrap();
        assert_eq!(load_origin(&config).unwrap(), Some(second));
        save_origin(&config, None).unwrap();
        save_origin(&config, None).unwrap();
        assert_eq!(load_origin(&config).unwrap(), None);
        assert_eq!(
            fs::read_to_string(config.join("other-setting")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn invalid_local_configuration_never_becomes_a_navigation_target() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("workspace-origin"), "file:///private").unwrap();
        assert!(load_origin(dir.path()).is_err());
        save_origin(dir.path(), None).unwrap();
        assert_eq!(load_origin(dir.path()).unwrap(), None);
    }

    #[test]
    fn untrusted_message_text_cannot_inject_markup_or_expand_template_tokens() {
        let html = launcher_html(None, Some("<img src=x> & {{MARK}}"), None);
        assert!(html.contains("&lt;img src=x&gt; &amp; {{MARK}}"));
        assert!(!html.contains("<img src=x>"));
    }
}
