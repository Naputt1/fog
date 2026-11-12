//! Tests for the logic shared with `build.rs`.
//!
//! A build script is compiled as a program, never with `--test`, so a
//! `#[cfg(test)]` module inside `build.rs` would never run. The testable pieces
//! live in `build/spa_lock.rs` and `build/spa_tar.rs` and are compiled here as
//! well. Nothing in this file touches the network: archives and locks are
//! fixtures built in memory or under a temp directory.

#[path = "../build/spa_lock.rs"]
mod spa_lock;
#[path = "../build/spa_tar.rs"]
mod spa_tar;

use std::fs;
use std::path::{Path, PathBuf};

use spa_lock::{parse_lock, select_entries, verify_checksum};

const HASH_V013: &str = "14bb132df1f670e376cffc6fa8462cd0e5d0bc7301a79ca50021edc0ef4afb1e";
const HASH_V012: &str = "eb3a7ea3d9578d3c414d94e1527628d9ac8ace62a2529765ca0d5a3e8b91e43f";

fn valid_lock() -> String {
    format!(
        "# pinned bundles\n\
         lock_version = 1\n\
         \n\
         [[spa]]\n\
         version = \"0.1.3\"\n\
         url = \"https://example.test/fog-spa-v0.1.3.tar.gz\"\n\
         sha256 = \"{HASH_V013}\"\n\
         \n\
         [[spa]]\n\
         version = \"0.1.2\"\n\
         url = \"https://example.test/fog-spa-v0.1.2.tar.gz\"\n\
         sha256 = \"{HASH_V012}\"\n"
    )
}

fn entry(version: &str, sha256: &str) -> spa_lock::LockEntry {
    spa_lock::LockEntry {
        version: version.to_string(),
        url: format!("https://example.test/fog-spa-v{version}.tar.gz"),
        sha256: sha256.to_string(),
    }
}

#[test]
fn select_keeps_the_file_order_for_unreleased_versions() {
    let entries = vec![
        entry("0.1.3", HASH_V013),
        entry("0.1.2", HASH_V012),
        entry("0.1.1", HASH_V013),
    ];
    let selected = select_entries(&entries, "9.9.9");
    let versions: Vec<&str> = selected.iter().map(|e| e.version.as_str()).collect();
    assert_eq!(versions, ["0.1.3", "0.1.2", "0.1.1"]);
}

#[test]
fn lock_parses_entries_in_file_order() {
    let entries = parse_lock(&valid_lock()).expect("valid lock");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].version, "0.1.3");
    assert_eq!(entries[0].url, "https://example.test/fog-spa-v0.1.3.tar.gz");
    assert_eq!(entries[0].sha256, HASH_V013);
    assert_eq!(entries[1].version, "0.1.2");
}

#[test]
fn lock_rejects_entry_without_version() {
    let text = format!(
        "lock_version = 1\n[[spa]]\nurl = \"https://example.test/a.tar.gz\"\nsha256 = \"{HASH_V013}\"\n"
    );
    let err = parse_lock(&text).expect_err("missing version must fail");
    assert!(err.contains("has no `version`"), "{err}");
}

#[test]
fn lock_rejects_malformed_line() {
    let text = format!("lock_version = 1\n[[spa]]\nversion 0.1.3\nsha256 = \"{HASH_V013}\"\n");
    let err = parse_lock(&text).expect_err("unparsable line must fail");
    assert!(err.contains("expected `[[spa]]`"), "{err}");
}

#[test]
fn lock_rejects_missing_schema_version() {
    let text = format!(
        "[[spa]]\nversion = \"0.1.3\"\nurl = \"https://e.test/a.tgz\"\nsha256 = \"{HASH_V013}\"\n"
    );
    let err = parse_lock(&text).expect_err("missing lock_version must fail");
    assert!(err.contains("lock_version"), "{err}");
}

#[test]
fn lock_rejects_plain_http_url() {
    let text = format!(
        "lock_version = 1\n[[spa]]\nversion = \"0.1.3\"\nurl = \"http://e.test/a.tgz\"\nsha256 = \"{HASH_V013}\"\n"
    );
    let err = parse_lock(&text).expect_err("http must fail");
    assert!(err.contains("https"), "{err}");
}

#[test]
fn lock_rejects_duplicate_version() {
    let text = format!(
        "lock_version = 1\n\
         [[spa]]\nversion = \"0.1.3\"\nurl = \"https://e.test/a.tgz\"\nsha256 = \"{HASH_V013}\"\n\
         [[spa]]\nversion = \"0.1.3\"\nurl = \"https://e.test/b.tgz\"\nsha256 = \"{HASH_V012}\"\n"
    );
    let err = parse_lock(&text).expect_err("duplicate version must fail");
    assert!(err.contains("duplicate entry"), "{err}");
}

#[test]
fn lock_rejects_unknown_key() {
    let text = "lock_version = 1\n[[spa]]\nbranch = \"main\"\n";
    let err = parse_lock(text).expect_err("unknown key must fail");
    assert!(err.contains("unknown key"), "{err}");
}

#[test]
fn select_prefers_the_exact_version() {
    let entries = parse_lock(&valid_lock()).expect("valid lock");
    let selected = select_entries(&entries, "0.1.2");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].version, "0.1.2");
}

#[test]
fn select_falls_back_to_newest_first_for_unreleased_versions() {
    let entries = parse_lock(&valid_lock()).expect("valid lock");
    let selected = select_entries(&entries, "0.2.0");
    let versions: Vec<&str> = selected.iter().map(|e| e.version.as_str()).collect();
    assert_eq!(versions, ["0.1.3", "0.1.2"]);
}

#[test]
fn checksum_accepts_a_matching_digest() {
    verify_checksum("fog-tui", HASH_V013, HASH_V013).expect("identical digests match");
    let upper = HASH_V013.to_ascii_uppercase();
    verify_checksum("fog-tui", &upper, HASH_V013).expect("hex case is irrelevant");
}

#[test]
fn checksum_reports_a_mismatch_with_crate_name_and_both_digests() {
    let actual = "deadbeef".repeat(8);
    let err = verify_checksum("fog-tui", HASH_V013, &actual).expect_err("mismatch must fail");
    assert!(err.contains("fog-tui"), "{err}");
    assert!(err.contains(HASH_V013), "{err}");
    assert!(err.contains(&actual), "{err}");
    assert!(err.contains("checksum mismatch"), "{err}");
}

#[test]
fn checksum_rejects_an_unfilled_placeholder() {
    let err = verify_checksum("fog-tui", "TODO", HASH_V013).expect_err("placeholder must fail");
    assert!(err.contains("no usable sha256"), "{err}");
    assert!(err.contains("fog-tui"), "{err}");
}

#[test]
fn safe_relative_path_accepts_plain_asset_names() {
    for name in [
        "index.html",
        "dist/index.html",
        "dist/assets/app.js",
        "./dist/a",
    ] {
        assert!(
            spa_tar::safe_relative_path(name).is_some(),
            "{name} should be accepted"
        );
    }
}

#[test]
fn safe_relative_path_rejects_escapes() {
    for name in [
        "../evil",
        "dist/../../evil",
        "/abs/path",
        "..",
        "C:/evil",
        "dist\\..\\evil",
        "",
    ] {
        assert!(
            spa_tar::safe_relative_path(name).is_none(),
            "{name} should be rejected"
        );
    }
}

#[test]
fn extract_writes_regular_files() {
    let dir = temp_dir("extract-ok");
    let archive = tar_archive(&[
        ("dist/", b"".as_slice(), b'5'),
        ("dist/index.html", b"<html>ok</html>", b'0'),
        ("dist/assets/app.js", b"console.log(1)", b'0'),
    ]);

    spa_tar::extract(&archive, &dir).expect("extract");
    assert_eq!(
        fs::read_to_string(dir.join("dist/index.html")).expect("index.html"),
        "<html>ok</html>"
    );
    assert_eq!(
        fs::read(dir.join("dist/assets/app.js")).expect("app.js"),
        b"console.log(1)"
    );
    remove_dir(&dir);
}

#[test]
fn extract_rejects_parent_traversal() {
    let root = temp_dir("extract-traversal");
    let dest = root.join("dest");
    fs::create_dir_all(&dest).expect("create dest");
    let archive = tar_archive(&[("../evil", b"pwned", b'0')]);

    let err = spa_tar::extract(&archive, &dest).expect_err("must reject");
    assert!(err.contains("not a safe relative path"), "{err}");
    assert!(!root.join("evil").exists());
    remove_dir(&root);
}

#[test]
fn extract_rejects_absolute_paths() {
    let dir = temp_dir("extract-absolute");
    let escaped = format!("/fog-build-test-{}-escape", std::process::id());
    let archive = tar_archive(&[(escaped.as_str(), b"pwned", b'0')]);

    let err = spa_tar::extract(&archive, &dir).expect_err("must reject");
    assert!(err.contains("not a safe relative path"), "{err}");
    assert!(!Path::new(&escaped).exists());
    remove_dir(&dir);
}

#[test]
fn extract_rejects_symlinks() {
    let dir = temp_dir("extract-symlink");
    let archive = tar_archive(&[("dist/link", b"/etc/passwd", b'2')]);

    let err = spa_tar::extract(&archive, &dir).expect_err("must reject");
    assert!(err.contains("unsupported type"), "{err}");
    assert!(!dir.join("dist/link").exists());
    remove_dir(&dir);
}

#[test]
fn extract_rejects_truncated_archives() {
    let dir = temp_dir("extract-truncated");
    let mut archive = tar_archive(&[("dist/index.html", &[b'x'; 600], b'0')]);
    archive.truncate(700);

    let err = spa_tar::extract(&archive, &dir).expect_err("must reject");
    assert!(err.contains("truncated"), "{err}");
    remove_dir(&dir);
}

#[test]
fn extract_honours_gnu_long_names() {
    let dir = temp_dir("extract-longname");
    let archive = tar_archive(&[
        ("././@LongLink", b"dist/real-name.html", b'L'),
        ("dist/truncated-name", b"<html>long</html>", b'0'),
    ]);

    spa_tar::extract(&archive, &dir).expect("extract");
    assert!(dir.join("dist/real-name.html").is_file());
    remove_dir(&dir);
}

#[test]
fn extract_honours_pax_path_records() {
    let dir = temp_dir("extract-pax");
    let mut pax = Vec::new();
    pax.extend(pax_record("path", "dist/index.html"));
    let archive = tar_archive(&[
        ("PaxHeaders/x", &pax, b'x'),
        ("placeholder", b"<html>pax</html>", b'0'),
    ]);

    spa_tar::extract(&archive, &dir).expect("extract");
    assert_eq!(
        fs::read_to_string(dir.join("dist/index.html")).expect("index.html"),
        "<html>pax</html>"
    );
    remove_dir(&dir);
}

#[test]
fn extract_size_cap_is_sane() {
    assert_eq!(spa_tar::MAX_EXTRACT_BYTES, 64 * 1024 * 1024);
}

/// A unique, empty directory under the system temp dir for one test.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("fog-{tag}-{}-{nanos}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn remove_dir(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

/// Build an uncompressed tar archive from `(name, data, typeflag)` entries.
fn tar_archive(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
    let mut archive = Vec::new();
    for (name, data, typeflag) in entries {
        archive.extend_from_slice(&tar_header(name, data.len(), *typeflag));
        archive.extend_from_slice(data);
        archive.extend(std::iter::repeat_n(
            0u8,
            data.len().div_ceil(512) * 512 - data.len(),
        ));
    }
    archive.extend_from_slice(&[0u8; 1024]);
    archive
}

fn tar_header(name: &str, size: usize, typeflag: u8) -> [u8; 512] {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut header[100..108], 0o644); // mode
    write_octal(&mut header[108..116], 0); // uid
    write_octal(&mut header[116..124], 0); // gid
    write_octal(&mut header[124..136], size as u64);
    write_octal(&mut header[136..148], 0); // mtime
    header[156] = typeflag;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    write_octal(&mut header[148..156], u64::from(checksum));
    header
}

fn write_octal(field: &mut [u8], value: u64) {
    let text = format!("{value:0width$o}", width = field.len() - 1);
    field[..text.len()].copy_from_slice(text.as_bytes());
    field[text.len()] = 0;
}

/// Encode one pax extended-header record: `<len> path=<value>\n`.
fn pax_record(key: &str, value: &str) -> Vec<u8> {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let candidate = format!("{len}{body}");
        if candidate.len() == len {
            return candidate.into_bytes();
        }
        len = candidate.len();
    }
}
