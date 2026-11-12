//! Build-time embedding of the frontend SPA (built by Vite into `ui/dist/`).
//!
//! The frontend is produced by a parallel worker and may not exist yet when this
//! crate is compiled. Because `rust-embed` / `include_dir!` panic at compile
//! time if the target directory is missing, we instead generate a Rust module at
//! build time:
//!
//!   - If `ui/dist/` exists, every file is copied into `$OUT_DIR/fog_assets/`
//!     and the generated module returns each file's bytes keyed by its URL path
//!     (with `index.html` also served at `/`).
//!   - If `ui/dist/` is absent, download the prebuilt tarball pinned in
//!     `spa.lock` for `CARGO_PKG_VERSION` (so `cargo install --git` works without
//!     Node), verify its SHA-256 against the lock, and embed that. Each entry of
//!     the tarball is checked before it is written, so the archive cannot escape
//!     the extraction directory.
//!   - When the download is impossible (offline, no release yet) the build
//!     fails and says how to fix it. Embedding a minimal fallback page instead is
//!     opt-in via `FOG_ALLOW_SPA_FALLBACK=1`; `FOG_REQUIRE_SPA=1` forbids it.
//!
//! The generated module is pulled into `src/index.rs` via
//! `include!(concat!(env!("OUT_DIR"), "/fog_embedded.rs"))`.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "build/spa_lock.rs"]
mod spa_lock;
#[path = "build/spa_tar.rs"]
mod spa_tar;

use spa_lock::{LockEntry, parse_lock, select_entries, verify_checksum};

/// A prebuilt SPA tarball is a few hundred KiB; anything this large is not one.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Why a pinned bundle could not be embedded.
enum SpaError {
    /// The bundle could not be obtained (offline, no release asset, no cache).
    /// Recoverable only by explicitly opting into the stub page.
    Unavailable(String),
    /// The pinned data itself is wrong: a checksum mismatch, a malformed lock
    /// or a malformed archive. Never silenced by `FOG_ALLOW_SPA_FALLBACK`.
    Fatal(String),
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ui_dist = manifest_dir.join("ui").join("dist");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let crate_name = env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "fog-tui".to_string());
    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());

    // Rebuild when the frontend output or the pin changes.
    println!("cargo:rerun-if-changed=ui/dist");
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed=spa.lock");
    println!("cargo:rerun-if-changed=build/spa_lock.rs");
    println!("cargo:rerun-if-changed=build/spa_tar.rs");
    println!("cargo:rerun-if-env-changed=FOG_SKIP_SPA_DOWNLOAD");
    println!("cargo:rerun-if-env-changed=FOG_REQUIRE_SPA");
    println!("cargo:rerun-if-env-changed=FOG_ALLOW_SPA_FALLBACK");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");

    // Fast path: local build present.
    if ui_dist.is_dir() {
        generate_from_dir(&ui_dist, &out_dir);
        return;
    }

    let require_spa = env::var_os("FOG_REQUIRE_SPA").is_some();
    let fallback_requested = env::var_os("FOG_ALLOW_SPA_FALLBACK").is_some();
    if require_spa && fallback_requested {
        warn_multiline(
            "FOG_REQUIRE_SPA=1 overrides FOG_ALLOW_SPA_FALLBACK=1: a missing SPA is a hard error",
        );
    }
    let fallback_allowed = fallback_requested && !require_spa;

    // Still read the cache when the network is off: a tarball that was verified
    // on an earlier build is usable offline, because it is re-checked here.
    let offline = env::var_os("FOG_SKIP_SPA_DOWNLOAD").is_some()
        || env::var_os("CARGO_NET_OFFLINE").is_some();

    match fetch_locked_spa(&manifest_dir, &out_dir, &crate_name, &pkg_version, !offline) {
        Ok(dist) => {
            generate_from_dir(&dist, &out_dir);
            println!("cargo:warning=embedded the prebuilt SPA from spa.lock");
        }
        Err(SpaError::Fatal(message)) => panic!("{message}"),
        Err(SpaError::Unavailable(message)) if fallback_allowed => {
            warn_multiline(&message);
            warn_multiline(
                "FOG_ALLOW_SPA_FALLBACK=1: embedding the fallback page instead of the full SPA",
            );
            generate_fallback(&out_dir, &manifest_dir);
        }
        Err(SpaError::Unavailable(message)) => panic!("{message}"),
    }
}

fn warn_multiline(message: &str) {
    for line in message.lines() {
        println!("cargo:warning={line}");
    }
}

/// Download (or reuse) the bundle pinned in `spa.lock` and unpack it.
///
/// Candidates come from the lock: the exact `CARGO_PKG_VERSION` when it has an
/// entry, otherwise every entry newest-first, which covers `cargo install --git`
/// on a main tip whose version is ahead of the published assets. Each candidate
/// is tied to its own digest, so the fallback never weakens verification.
fn fetch_locked_spa(
    manifest_dir: &Path,
    out_dir: &Path,
    crate_name: &str,
    pkg_version: &str,
    allow_network: bool,
) -> Result<PathBuf, SpaError> {
    let lock_path = manifest_dir.join("spa.lock");
    let text = fs::read_to_string(&lock_path).map_err(|err| {
        SpaError::Fatal(format!(
            "{crate_name}: cannot read {}: {err}\n\
             ui/dist is absent, so build.rs needs spa.lock to know which prebuilt SPA to\n\
             download and which SHA-256 it must have.\n\
             Fix: restore spa.lock (`git checkout -- spa.lock`), or build the frontend:\n\
             \x20 cd ui && bun install && bun run build",
            lock_path.display()
        ))
    })?;

    let entries = parse_lock(&text).map_err(|err| {
        SpaError::Fatal(format!(
            "{crate_name}: spa.lock is malformed: {err}\n\
             Fix: restore spa.lock (`git checkout -- spa.lock`), or build the frontend:\n\
             \x20 cd ui && bun install && bun run build"
        ))
    })?;

    let candidates = select_entries(&entries, pkg_version);
    // parse_lock rejects an empty file, so there is always a candidate; the
    // first one is only the exact match or the newest entry.
    if let Some(newest) = candidates.first()
        && newest.version != pkg_version
    {
        warn_multiline(&format!(
            "{crate_name}: spa.lock has no entry for v{pkg_version}; trying the pinned bundles \
             that exist, newest first (starting with v{})",
            newest.version
        ));
    }

    let mut failures: Vec<String> = Vec::new();
    for entry in &candidates {
        match fetch_entry(out_dir, entry, crate_name, allow_network) {
            Ok(dist) => return Ok(dist),
            Err(SpaError::Fatal(message)) => return Err(SpaError::Fatal(message)),
            Err(SpaError::Unavailable(message)) => failures.push(message),
        }
    }

    let mut message = format!(
        "{crate_name}: ui/dist is absent and no pinned SPA bundle could be obtained for v{pkg_version}.\n\
         Tried, from spa.lock:\n"
    );
    for entry in &candidates {
        message.push_str(&format!("  - {} (sha256 {})\n", entry.url, entry.sha256));
    }
    message.push_str("Problems:\n");
    for failure in &failures {
        message.push_str(&format!("  - {failure}\n"));
    }
    message.push_str(
        "Nothing was embedded, because silently serving a stub page hides a broken build.\n\
         Fix one of:\n\
         \x20 * build the frontend:  cd ui && bun install && bun run build\n\
         \x20 * restore the network: the tarballs are on https://github.com/Naputt1/fog/releases\n\
         \x20 * accept a stub UI:    FOG_ALLOW_SPA_FALLBACK=1",
    );
    Err(SpaError::Unavailable(message))
}

/// Resolve one lock entry to a directory containing `index.html`.
fn fetch_entry(
    out_dir: &Path,
    entry: &LockEntry,
    crate_name: &str,
    allow_network: bool,
) -> Result<PathBuf, SpaError> {
    let cache = cache_dir(out_dir).join(format!("fog-spa-v{}.tar.gz", entry.version));

    if cache.is_file() {
        let verified = sha256_file(&cache)
            .map(|actual| verify_checksum(crate_name, &entry.sha256, &actual).is_ok())
            .unwrap_or(false);
        if verified {
            println!(
                "cargo:warning=reusing cached SPA tarball {} (sha256 matches spa.lock)",
                cache.display()
            );
            return extract_bundle(&cache, out_dir, entry, crate_name);
        }
        // Partial, stale or unpinned cache file: never embed it, fetch again.
        let _ = fs::remove_file(&cache);
    }

    if !allow_network {
        return Err(SpaError::Unavailable(format!(
            "{} is not cached, and downloads are disabled \
             (FOG_SKIP_SPA_DOWNLOAD or CARGO_NET_OFFLINE is set)",
            entry.url
        )));
    }

    let partial = cache.with_file_name(format!("fog-spa-v{}.tar.gz.part", entry.version));
    if !download_to(&entry.url, &partial) {
        let _ = fs::remove_file(&partial);
        return Err(SpaError::Unavailable(format!(
            "download failed: {} (offline, 404, or neither curl nor wget is installed)",
            entry.url
        )));
    }

    let size = fs::metadata(&partial).map(|meta| meta.len()).unwrap_or(0);
    if size > MAX_DOWNLOAD_BYTES {
        let _ = fs::remove_file(&partial);
        return Err(SpaError::Fatal(format!(
            "{crate_name}: {} is {size} bytes, above the {MAX_DOWNLOAD_BYTES}-byte limit for an \
             SPA bundle",
            entry.url
        )));
    }

    let actual = match sha256_file(&partial) {
        Ok(actual) => actual,
        Err(err) => {
            let _ = fs::remove_file(&partial);
            return Err(SpaError::Unavailable(format!(
                "cannot read the downloaded tarball {}: {err}",
                entry.url
            )));
        }
    };
    if let Err(message) = verify_checksum(crate_name, &entry.sha256, &actual) {
        let _ = fs::remove_file(&partial);
        return Err(SpaError::Fatal(message));
    }

    let dist = extract_bundle(&partial, out_dir, entry, crate_name)?;
    // Keep the verified tarball for the next build. Extraction already
    // succeeded, so a failed rename only costs a re-download.
    let _ = fs::rename(&partial, &cache);
    Ok(dist)
}

/// Download `url` to `dest`.
///
/// curl is preferred, wget is the fallback. Either is only a transport: the
/// bytes are hashed against `spa.lock` afterwards, so neither the tool nor the
/// server is trusted to decide what gets embedded.
fn download_to(url: &str, dest: &Path) -> bool {
    for (program, flags) in [("curl", &["-fsSL", "-o"][..]), ("wget", &["-qO"][..])] {
        let mut cmd = Command::new(program);
        cmd.args(flags)
            .arg(dest)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Ok(status) = cmd.status()
            && status.success()
            && fs::metadata(dest)
                .map(|meta| meta.len() > 0)
                .unwrap_or(false)
        {
            return true;
        }
        let _ = fs::remove_file(dest);
    }
    false
}

fn extract_bundle(
    tar_path: &Path,
    out_dir: &Path,
    entry: &LockEntry,
    crate_name: &str,
) -> Result<PathBuf, SpaError> {
    let compressed = fs::read(tar_path).map_err(|err| {
        SpaError::Fatal(format!(
            "{crate_name}: cannot read {}: {err}",
            tar_path.display()
        ))
    })?;

    let mut archive = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(spa_tar::MAX_EXTRACT_BYTES + 1)
        .read_to_end(&mut archive)
        .map_err(|err| {
            SpaError::Fatal(format!(
                "{crate_name}: {} is not a valid gzip stream: {err}",
                entry.url
            ))
        })?;
    if archive.len() as u64 > spa_tar::MAX_EXTRACT_BYTES {
        return Err(SpaError::Fatal(format!(
            "{crate_name}: {} unpacks to more than {} bytes; refusing to extract",
            entry.url,
            spa_tar::MAX_EXTRACT_BYTES
        )));
    }

    let extract_dir = out_dir.join("fog-spa-extract");
    let _ = fs::remove_dir_all(&extract_dir);
    fs::create_dir_all(&extract_dir).map_err(|err| {
        SpaError::Fatal(format!(
            "{crate_name}: cannot create {}: {err}",
            extract_dir.display()
        ))
    })?;

    spa_tar::extract(&archive, &extract_dir).map_err(|err| {
        SpaError::Fatal(format!(
            "{crate_name}: refusing the SPA bundle {}: {err}\n\
             Every entry must be a regular file or directory inside the archive.\n\
             Fix: build the frontend instead (cd ui && bun install && bun run build), or\n\
             re-publish fog-spa-v{}.tar.gz and update spa.lock.",
            entry.url, entry.version
        ))
    })?;

    find_dist_dir(&extract_dir).ok_or_else(|| {
        SpaError::Fatal(format!(
            "{crate_name}: the SPA bundle {} contains no index.html, so there is nothing to \
             embed.\n\
             Fix: re-publish fog-spa-v{}.tar.gz from a `bun run build` output, or build the\n\
             frontend instead: cd ui && bun install && bun run build",
            entry.url, entry.version
        ))
    })
}

/// Locate the `dist` directory inside an extracted bundle. The release tarball
/// is `tar czf ... -C ui dist`, but older layouts are still recognised.
fn find_dist_dir(extract_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        extract_dir.join("dist"),
        extract_dir.join("ui").join("dist"),
        extract_dir.to_path_buf(),
    ];
    for candidate in candidates {
        if candidate.join("index.html").is_file() {
            return Some(candidate);
        }
    }
    fs::read_dir(extract_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("index.html").is_file())
}

fn cache_dir(out_dir: &Path) -> PathBuf {
    let preferred = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("cache").join("fog"))
        .unwrap_or_else(|| out_dir.join("spa-cache"));
    if fs::create_dir_all(&preferred).is_ok() {
        return preferred;
    }
    // Read-only CARGO_HOME: fall back to the build directory.
    let fallback = out_dir.join("spa-cache");
    let _ = fs::create_dir_all(&fallback);
    fallback
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

fn generate_from_dir(ui_dist: &Path, out_dir: &Path) {
    let assets_dir = out_dir.join("fog_assets");
    let _ = fs::remove_dir_all(&assets_dir);
    fs::create_dir_all(&assets_dir).expect("create fog_assets dir");

    let mut arms: Vec<String> = Vec::new();
    collect_assets(ui_dist, "", &assets_dir, &mut arms);

    let mut content = String::new();
    content.push_str(
        "// Generated by build.rs — embeds the frontend build from ui/dist.\n\
  // Each arm carries the correct content-type computed from the *real* asset\n\
  // filename (via `mime_for`), never from the request path. This matters for\n\
  // the root alias `/` (which maps to `index.html`): computing the mime from\n\
  // the request path `/` would yield `application/octet-stream` and make the\n\
  // browser download the page instead of rendering it.\n\
  //\n\
  // Text assets (JS/CSS/HTML/SVG/JSON) also embed precompressed gzip + brotli\n\
  // variants so the server can serve `Content-Encoding: br/gzip` with zero\n\
  // runtime CPU cost. `EmbeddedAsset` itself is defined in `src/index.rs`.\n\
  pub fn embedded_get(path: &str) -> Option<EmbeddedAsset> {\n    let asset = match path {\n",
    );
    for arm in &arms {
        content.push_str(arm);
        content.push('\n');
    }
    content.push_str("        _ => return None,\n    };\n    Some(asset)\n}\n");

    fs::write(out_dir.join("fog_embedded.rs"), content).expect("write fog_embedded.rs");
}

fn generate_fallback(out_dir: &Path, manifest_dir: &Path) {
    let fallback_src = manifest_dir.join("src").join("fallback.html");
    let fallback_bytes =
        fs::read(&fallback_src).unwrap_or_else(|_| b"<h1>fog UI not built</h1>".to_vec());
    let fallback_html_path = out_dir.join("fallback.html");
    fs::write(&fallback_html_path, &fallback_bytes).expect("write fallback.html");

    // Embed fallback for "/" and "/index.html" so serve_spa_fallback returns 200.
    // Use mime_for("index.html") so Content-Type is text/html.
    // `EmbeddedAsset` is defined in `src/index.rs`; the fallback is tiny HTML
    // so precompressed variants are omitted (None).
    let content = r#"// Generated by build.rs — fallback when ui/dist absent.
pub fn embedded_get(path: &str) -> Option<EmbeddedAsset> {
    const FALLBACK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fallback.html"));
    match path {
        "/" | "/index.html" => Some(EmbeddedAsset {
            data: FALLBACK,
            mime: mime_for("index.html"),
            gzip: None,
            br: None,
        }),
        _ => None,
    }
}
"#;
    fs::write(out_dir.join("fog_embedded.rs"), content).expect("write fog_embedded.rs");
}

/// Extensions worth precompressing: text formats that shrink well with
/// gzip/brotli. Already-compressed formats (images, fonts, wasm) are skipped
/// so the binary doesn't pay for encodings that save nothing.
fn is_compressible(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    matches!(
        ext,
        "html"
            | "htm"
            | "js"
            | "mjs"
            | "css"
            | "json"
            | "map"
            | "svg"
            | "txt"
            | "webmanifest"
            | "xml"
    )
}

/// gzip-compresses `data` at best level. Returns `None` when compression
/// fails or doesn't shrink the input.
fn gzip_compress(data: &[u8]) -> Option<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    enc.write_all(data).ok()?;
    let out = enc.finish().ok()?;
    (out.len() < data.len()).then_some(out)
}

/// brotli-compresses `data` at quality 11. Returns `None` when compression
/// fails or doesn't shrink the input.
fn brotli_compress(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut out = Vec::new();
    {
        let mut enc = brotli::CompressorWriter::new(&mut out, 4096, 11, 22);
        enc.write_all(data).ok()?;
        enc.flush().ok()?;
    }
    (out.len() < data.len()).then_some(out)
}

/// Recursively walks `dir`, copying each file into `assets_dir` (plus gzip +
/// brotli variants for compressible files) and emitting a match arm mapping
/// its URL path to an `EmbeddedAsset` of embedded bytes.
fn collect_assets(dir: &Path, prefix: &str, assets_dir: &Path, arms: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_assets(&path, &rel, assets_dir, arms);
            continue;
        }
        let encoded = rel.replace('/', "__");
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        if fs::write(assets_dir.join(&encoded), &bytes).is_err() {
            continue;
        }
        let include =
            format!("include_bytes!(concat!(env!(\"OUT_DIR\"), \"/fog_assets/\", \"{encoded}\"))");
        // Precompressed variants live next to the raw file. Each is embedded
        // only when it actually shrinks the input; otherwise the arm carries
        // `None` and the server falls back to identity.
        let mut gzip_arm = "None".to_string();
        let mut br_arm = "None".to_string();
        if is_compressible(&rel) {
            if let Some(gz) = gzip_compress(&bytes) {
                let gz_name = format!("{encoded}.gz");
                if fs::write(assets_dir.join(&gz_name), &gz).is_ok() {
                    gzip_arm = format!(
                        "Some(include_bytes!(concat!(env!(\"OUT_DIR\"), \"/fog_assets/\", \"{gz_name}\")) as &[u8])"
                    );
                }
            }
            if let Some(br) = brotli_compress(&bytes) {
                let br_name = format!("{encoded}.br");
                if fs::write(assets_dir.join(&br_name), &br).is_ok() {
                    br_arm = format!(
                        "Some(include_bytes!(concat!(env!(\"OUT_DIR\"), \"/fog_assets/\", \"{br_name}\")) as &[u8])"
                    );
                }
            }
        }
        let url_path = format!("/{rel}");
        // Coerce each arm's `&[u8; N]` to `&[u8]` so all match arms share one
        // type (include_bytes! yields a distinct array size per file, which
        // would otherwise be a compile error). The mime is derived from the
        // real filename (`rel`), so the root alias below maps to `index.html`'s
        // text/html rather than the request path's octet-stream.
        let arm_body = format!(
            "EmbeddedAsset {{ data: {include} as &[u8], mime: mime_for({rel:?}), gzip: {gzip_arm}, br: {br_arm} }}"
        );
        arms.push(format!("        {url_path:?} => {arm_body},"));
        // Serve `index.html` at the root too (SPA entry point).
        if rel == "index.html" {
            arms.push(format!("        \"/\" => {arm_body},"));
        }
    }
}
