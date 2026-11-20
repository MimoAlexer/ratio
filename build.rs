use std::{env, fs, io::Write, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=assets");
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("assets");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("bundled_assets.rs");
    let mut files = Vec::new();
    // Only browser assets in the public directory and the shipped font directory.
    for directory in [root.clone(), root.join("fonts")] {
        for entry in fs::read_dir(directory).expect("read public assets") {
            let entry = entry.expect("read public asset");
            if !entry.file_type().expect("asset type").is_file() {
                continue;
            }
            let p = entry.path();
            let mime = match p.extension().and_then(|s| s.to_str()) {
                Some("css") => "text/css; charset=utf-8",
                Some("js") => "text/javascript; charset=utf-8",
                Some("svg") => "image/svg+xml",
                Some("woff2") => "font/woff2",
                _ => continue,
            };
            let name = p
                .strip_prefix(&root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            let bytes = fs::read(&p).expect("read asset bytes");
            // HTTP cache validation only; this is not an integrity credential.
            let tag = bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| {
                (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            });
            files.push((name, mime, tag));
        }
    }
    files.sort_unstable();
    let mut generated = fs::File::create(out).expect("create asset table");
    writeln!(generated,"fn bundled_asset(key:&str)->Option<(&'static str,&'static [u8],&'static str)>{{ match key {{").unwrap();
    for (name, mime, tag) in files {
        let path = format!("/assets/{name}");
        let etag = format!("W/\"{tag:016x}\"");
        writeln!(generated, "{name:?}=>Some(({mime:?},include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"),{path:?})),{etag:?})),").unwrap();
    }
    writeln!(generated, "_=>None,}} }}").unwrap();
}
