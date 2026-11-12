//! Minimal tar reader for the pinned SPA bundle.
//!
//! `build.rs` unpacks `fog-spa-vX.tar.gz` itself instead of shelling out to
//! `tar`, so that every entry can be checked before it is written: only regular
//! files and directories are accepted, and no name may be absolute, contain
//! `..`, contain a backslash or name a Windows drive. Anything else aborts the
//! whole extraction.
//!
//! The archive arrives already verified against `spa.lock` (SHA-256), so this
//! is defence in depth rather than the only guard.
//!
//! Like `spa_lock`, this file is compiled by both `build.rs` and
//! `tests/build_support.rs` because a build script's own tests never run.

use std::fs;
use std::path::{Path, PathBuf};

const BLOCK: usize = 512;

/// Upper bound on the bytes unpacked from one bundle. The real SPA unpacked is
/// around 1 MiB; the cap stops a corrupt or hostile archive from filling a disk.
pub const MAX_EXTRACT_BYTES: u64 = 64 * 1024 * 1024;

/// Unpack a tar archive (already gunzipped) into `dest`.
pub fn extract(archive: &[u8], dest: &Path) -> Result<(), String> {
    let mut pos = 0usize;
    let mut written: u64 = 0;
    // GNU 'L' and pax 'x' headers name the entry that follows them.
    let mut pending_name: Option<String> = None;

    while pos + BLOCK <= archive.len() {
        let header = &archive[pos..pos + BLOCK];
        pos += BLOCK;
        if header.iter().all(|byte| *byte == 0) {
            break; // end-of-archive marker
        }
        let size = parse_size(&header[124..136])?;
        let data = payload(archive, &mut pos, size)?;
        let typeflag = header[156];
        let header_name = cstring(&header[0..100]);

        match typeflag {
            b'L' => {
                pending_name = Some(cstring(data));
                continue;
            }
            b'x' => {
                if let Some(path) = pax_path(data) {
                    pending_name = Some(path);
                }
                continue;
            }
            b'g' => {
                return Err(
                    "SPA tarball contains a pax global header, which is not accepted".to_string(),
                );
            }
            b'0' | 0 | b'5' => {}
            other => {
                let name = pending_name.as_deref().unwrap_or(&header_name);
                return Err(format!(
                    "SPA tarball entry {name:?} has unsupported type {:?}; only regular files \
                     and directories are accepted",
                    other as char
                ));
            }
        }

        let name = pending_name.take().unwrap_or(header_name);
        let Some(relative) = safe_relative_path(&name) else {
            return Err(format!(
                "SPA tarball entry {name:?} is not a safe relative path; absolute paths, `..` \
                 components and backslashes are rejected"
            ));
        };
        let target = dest.join(&relative);

        if typeflag == b'5' {
            fs::create_dir_all(&target)
                .map_err(|err| format!("cannot create {:?}: {err}", target.display()))?;
            continue;
        }

        written = written
            .checked_add(size)
            .ok_or_else(|| "SPA tarball size overflow".to_string())?;
        if written > MAX_EXTRACT_BYTES {
            return Err(format!(
                "SPA tarball unpacks to more than {MAX_EXTRACT_BYTES} bytes; refusing to extract"
            ));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {:?}: {err}", parent.display()))?;
        }
        fs::write(&target, data)
            .map_err(|err| format!("cannot write {:?}: {err}", target.display()))?;
    }
    Ok(())
}

/// Normalise an archive entry name into a path relative to the extraction
/// directory, or `None` when it could escape that directory.
pub fn safe_relative_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') {
        return None;
    }
    // A backslash is never a separator in tar. Treating it as data would make
    // the same entry mean different things on Unix and Windows.
    if name.contains('\\') || name.contains('\0') {
        return None;
    }
    let mut relative = PathBuf::new();
    let mut saw_component = false;
    for component in name.split('/') {
        match component {
            "" | "." => continue,
            ".." => return None,
            other => {
                let bytes = other.as_bytes();
                if bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                    return None; // Windows drive prefix such as "C:"
                }
                relative.push(other);
                saw_component = true;
            }
        }
    }
    saw_component.then_some(relative)
}

fn payload<'a>(archive: &'a [u8], pos: &mut usize, size: u64) -> Result<&'a [u8], String> {
    let size = usize::try_from(size)
        .map_err(|_| "SPA tarball entry is too large to unpack".to_string())?;
    let end = pos
        .checked_add(size)
        .ok_or_else(|| "SPA tarball entry offset overflow".to_string())?;
    if end > archive.len() {
        return Err(format!(
            "SPA tarball is truncated: an entry needs {size} bytes at offset {pos}"
        ));
    }
    let data = &archive[*pos..end];
    *pos += size.div_ceil(BLOCK) * BLOCK;
    Ok(data)
}

fn parse_size(field: &[u8]) -> Result<u64, String> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err("SPA tarball uses a base-256 size field, which is not accepted".to_string());
    }
    let digits: Vec<u8> = field
        .iter()
        .skip_while(|byte| **byte == 0 || **byte == b' ')
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .copied()
        .collect();
    let text = std::str::from_utf8(&digits)
        .map_err(|_| "SPA tarball has a non-ASCII size field".to_string())?;
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8)
        .map_err(|_| format!("SPA tarball has an invalid octal size field {text:?}"))
}

fn cstring(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Read a `path=` record from a pax extended header, if present.
fn pax_path(data: &[u8]) -> Option<String> {
    let mut rest = data;
    while let Some(space) = rest.iter().position(|byte| *byte == b' ') {
        let len: usize = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
        if len < 2 || len > rest.len() {
            return None;
        }
        let record = &rest[space + 1..len - 1];
        rest = &rest[len..];
        if let Some(value) = record.strip_prefix(b"path=") {
            return Some(String::from_utf8_lossy(value).into_owned());
        }
    }
    None
}
