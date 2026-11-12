//! Lock-file parsing and checksum policy for the prebuilt SPA download.
//!
//! This lives in its own file because a build script cannot host tests that
//! `cargo test` runs: build scripts are compiled as a program, never with
//! `--test`. The module is compiled by `build.rs` (`#[path] mod`) and by
//! `tests/build_support.rs`, which is where the tests are.
//!
//! `spa.lock` is deliberately tiny so parsing it needs no TOML dependency:
//!
//! ```toml
//! lock_version = 1
//!
//! [[spa]]
//! version = "0.1.3"
//! url = "https://github.com/.../fog-spa-v0.1.3.tar.gz"
//! sha256 = "<64 hex characters>"
//! ```

/// One published SPA bundle (`fog-spa-vX.tar.gz`) pinned by URL and digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockEntry {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// The lock-file schema this build understands.
const SUPPORTED_LOCK_VERSION: u32 = 1;

#[derive(Default)]
struct PartialEntry {
    line: usize,
    version: Option<String>,
    url: Option<String>,
    sha256: Option<String>,
}

impl PartialEntry {
    fn new(line: usize) -> Self {
        Self {
            line,
            ..Default::default()
        }
    }

    fn set(&mut self, key: &str, value: &str, line_no: usize) -> Result<(), String> {
        let slot = match key {
            "version" => &mut self.version,
            "url" => &mut self.url,
            "sha256" => &mut self.sha256,
            other => {
                return Err(format!(
                    "line {line_no}: unknown key {other:?} in [[spa]] (expected version, url, sha256)"
                ));
            }
        };
        if slot.is_some() {
            return Err(format!("line {line_no}: duplicate key {key:?} in [[spa]]"));
        }
        *slot = Some(unquote(value, line_no)?);
        Ok(())
    }

    fn finish(self) -> Result<LockEntry, String> {
        let version = self
            .version
            .ok_or_else(|| format!("[[spa]] starting on line {} has no `version`", self.line))?;
        if version.is_empty() || version.chars().any(char::is_whitespace) {
            return Err(format!(
                "[[spa]] starting on line {} has an invalid version {version:?}",
                self.line
            ));
        }
        let url = self
            .url
            .ok_or_else(|| format!("[[spa]] for v{version} has no `url`"))?;
        // A pinned digest over an unauthenticated transport protects nothing.
        if !url.starts_with("https://") {
            return Err(format!(
                "[[spa]] for v{version} must use an https:// url, found {url:?}"
            ));
        }
        let sha256 = self
            .sha256
            .ok_or_else(|| format!("[[spa]] for v{version} has no `sha256`"))?;
        Ok(LockEntry {
            version,
            url,
            sha256,
        })
    }
}

/// Parse `spa.lock` into entries, in file order (newest first by convention).
pub fn parse_lock(text: &str) -> Result<Vec<LockEntry>, String> {
    let mut entries: Vec<LockEntry> = Vec::new();
    let mut current: Option<PartialEntry> = None;
    let mut lock_version: Option<u32> = None;

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[spa]]" {
            if let Some(partial) = current.take() {
                entries.push(partial.finish()?);
            }
            current = Some(PartialEntry::new(line_no));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {line_no}: expected `[[spa]]` or `key = \"value\"`, found {line:?}"
            ));
        };
        let key = key.trim();
        let value = value.trim();
        match current.as_mut() {
            Some(partial) => partial.set(key, value, line_no)?,
            None => {
                if key != "lock_version" {
                    return Err(format!(
                        "line {line_no}: unknown top-level key {key:?} (expected `lock_version` or `[[spa]]`)"
                    ));
                }
                let parsed: u32 = value.parse().map_err(|_| {
                    format!("line {line_no}: `lock_version` must be an integer, found {value:?}")
                })?;
                if parsed != SUPPORTED_LOCK_VERSION {
                    return Err(format!(
                        "line {line_no}: unsupported lock_version {parsed} (this build understands {SUPPORTED_LOCK_VERSION})"
                    ));
                }
                if lock_version.replace(parsed).is_some() {
                    return Err(format!("line {line_no}: duplicate `lock_version`"));
                }
            }
        }
    }

    if let Some(partial) = current.take() {
        entries.push(partial.finish()?);
    }
    if lock_version.is_none() {
        return Err("missing `lock_version = 1`".to_string());
    }
    if entries.is_empty() {
        return Err("no `[[spa]]` entries".to_string());
    }
    for (idx, entry) in entries.iter().enumerate() {
        if entries[..idx].iter().any(|e| e.version == entry.version) {
            return Err(format!("duplicate entry for v{}", entry.version));
        }
    }
    Ok(entries)
}

fn unquote(value: &str, line_no: usize) -> Result<String, String> {
    let Some(rest) = value.strip_prefix('"') else {
        return Err(format!(
            "line {line_no}: expected a quoted string, found {value:?}"
        ));
    };
    let Some(inner) = rest.strip_suffix('"') else {
        return Err(format!("line {line_no}: unterminated string {value:?}"));
    };
    if inner.contains('"') {
        return Err(format!("line {line_no}: unexpected quote in {value:?}"));
    }
    Ok(inner.to_string())
}

/// Pick the entries to try, in order.
///
/// Prefer the exact package version. When that version has no release yet —
/// `cargo install --git` on a main tip where `CARGO_PKG_VERSION` is ahead of
/// the published assets — fall back to the whole lock in file order, which is
/// newest first. Every candidate is still tied to its own pinned checksum.
pub fn select_entries<'a>(entries: &'a [LockEntry], pkg_version: &str) -> Vec<&'a LockEntry> {
    let exact: Vec<&LockEntry> = entries
        .iter()
        .filter(|entry| entry.version == pkg_version)
        .collect();
    if exact.is_empty() {
        entries.iter().collect()
    } else {
        exact
    }
}

/// Check a downloaded file's digest against the pinned one.
///
/// Returns the message to show the user on failure; writing the message here
/// keeps it testable. A digest that is not 64 hex characters (an unfilled
/// placeholder, or a typo) is reported separately from a real mismatch.
pub fn verify_checksum(crate_name: &str, expected: &str, actual: &str) -> Result<(), String> {
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{crate_name}: spa.lock has no usable sha256 for this SPA tarball (found {expected:?})\n\
             expected 64 hex characters.\n\
             Fix: publish or pin the release asset first, then record its digest:\n\
             \x20 curl -fsSL <url> -o /tmp/fog-spa.tgz && shasum -a 256 /tmp/fog-spa.tgz\n\
             and put the output in spa.lock. Meanwhile build the frontend instead:\n\
             \x20 cd ui && bun install && bun run build"
        ));
    }
    if expected.eq_ignore_ascii_case(actual) {
        return Ok(());
    }
    Err(format!(
        "{crate_name}: SPA tarball checksum mismatch\n\
         \x20 expected (spa.lock): {expected}\n\
         \x20 actual   (download): {actual}\n\
         Refusing to embed a bundle that does not match spa.lock. Either the release\n\
         asset was replaced or spa.lock is out of date; do not edit the hash unless you\n\
         verified the tarball by hand. To build without the download instead:\n\
         \x20 cd ui && bun install && bun run build"
    ))
}
