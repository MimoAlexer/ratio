#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod connect;
mod runtime;
mod runtime_download;
mod runtime_platform;
mod runtime_profile;

use std::{ffi::OsString, path::PathBuf};

use connect::ServerOrigin;

const HELP: &str = "Ratio desktop\n\nUsage: ratio [--server ORIGIN] [--profile-dir DIRECTORY]\n\n  --server ORIGIN           Connect to and remember this Ratio deployment\n  --profile-dir DIRECTORY   Use a separate local profile directory\n  -h, --help                Show this help\n  -V, --version             Print the version\n\nUse an HTTPS origin, or HTTP on loopback for local development.";

struct Options {
    origin: Option<ServerOrigin>,
    profile: Option<PathBuf>,
}

enum Invocation {
    Help,
    Version,
    Run(Options),
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, String> {
    let mut options = Options {
        origin: None,
        profile: None,
    };
    let mut args = args.into_iter();
    while let Some(option) = args.next() {
        match option.to_str() {
            Some("-h" | "--help") => return Ok(Invocation::Help),
            Some("-V" | "--version") => return Ok(Invocation::Version),
            Some("--server") if options.origin.is_none() => {
                let value = args
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or("--server requires a Ratio origin")?;
                options.origin = Some(ServerOrigin::parse(&value)?);
            }
            Some("--profile-dir") if options.profile.is_none() => {
                let value = args
                    .next()
                    .filter(|value| !value.is_empty())
                    .ok_or("--profile-dir requires a directory")?;
                options.profile = Some(PathBuf::from(value));
            }
            _ => {
                return Err(format!(
                    "Unknown or repeated option {}. Use --help for usage.",
                    option.to_string_lossy()
                ));
            }
        }
    }
    Ok(Invocation::Run(options))
}

fn main() {
    let invocation = parse_args(std::env::args_os().skip(1)).unwrap_or_else(|error| {
        eprintln!("Ratio: {error}");
        std::process::exit(2);
    });
    let result = match invocation {
        Invocation::Help => {
            println!("{HELP}");
            return;
        }
        Invocation::Version => {
            println!("Ratio {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Invocation::Run(options) => runtime::run(options),
    };
    if let Err(error) = result {
        eprintln!("Ratio: {error}");
        runtime_platform::error(None, "Ratio could not start", &error);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_and_version_need_no_profile_or_display() {
        assert!(matches!(
            parse_args(["--help".into()]),
            Ok(Invocation::Help)
        ));
        assert!(matches!(
            parse_args(["--version".into()]),
            Ok(Invocation::Version)
        ));
    }

    #[test]
    fn options_reject_missing_values_and_repeated_flags() {
        for args in [
            vec!["--server"],
            vec!["--profile-dir"],
            vec!["--profile-dir", "a", "--profile-dir", "b"],
            vec!["--unknown"],
        ] {
            assert!(parse_args(args.into_iter().map(OsString::from)).is_err());
        }
    }
}
