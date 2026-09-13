//! Bounded export of Vellum-owned diagnostic logs.
//!
//! Only retained text log files are eligible. Product configuration,
//! credentials, conversation history, and databases are never traversed into
//! the archive. Every included file is sanitized again during export.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_BUNDLE_BYTES: usize = 128 * 1024 * 1024;
const MAX_TEXT_LINE_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SupportBundleError {
    #[error("support bundle I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("support bundle ZIP failed: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("support bundle manifest failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleManifest {
    schema_version: u32,
    created_at: String,
    limits: BundleLimits,
    files: Vec<BundleFile>,
    skipped: Vec<SkippedFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleLimits {
    max_file_bytes: u64,
    max_bundle_bytes: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BundleFile {
    archive_path: String,
    original_bytes: u64,
    exported_bytes: usize,
    truncated: bool,
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkippedFile {
    archive_path: String,
    reason: String,
}

struct Candidate {
    path: PathBuf,
    archive_path: String,
}

pub fn export(
    data_root: &Path,
    app_log_root: Option<&Path>,
    destination: &Path,
    diagnostic_log: Option<(String, bool)>,
) -> Result<PathBuf, SupportBundleError> {
    fs::create_dir_all(destination)?;
    let output = destination.join(format!(
        "vellum-logs-{}-{}.zip",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        ulid::Ulid::new()
    ));
    let output_canonical = output.canonicalize().unwrap_or_else(|_| output.clone());

    let mut candidates = Vec::new();
    collect_logs(data_root, "data", &mut candidates)?;
    if let Some(root) = app_log_root {
        collect_logs(root, "app", &mut candidates)?;
    }
    candidates.sort_by(|left, right| left.archive_path.cmp(&right.archive_path));

    let file = File::create(&output)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let replacements = path_replacements(data_root);
    let mut seen = HashSet::new();
    let mut exported_total = 0usize;
    let mut files = Vec::new();
    let mut skipped = Vec::new();

    if let Some((diagnostics, truncated)) = diagnostic_log {
        let archive_path = "data/diagnostic-events.jsonl";
        let sanitized = sanitize_text(&diagnostics, &replacements);
        zip.start_file(archive_path, options)?;
        zip.write_all(sanitized.as_bytes())?;
        exported_total = sanitized.len();
        files.push(BundleFile {
            archive_path: archive_path.into(),
            original_bytes: diagnostics.len() as u64,
            exported_bytes: sanitized.len(),
            truncated,
            sha256: hex::encode(Sha256::digest(sanitized.as_bytes())),
        });
    }

    for candidate in candidates {
        let canonical = match candidate.path.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                skipped.push(SkippedFile {
                    archive_path: candidate.archive_path,
                    reason: format!("cannot resolve file: {error}"),
                });
                continue;
            }
        };
        if canonical == output_canonical || !seen.insert(canonical.clone()) {
            continue;
        }
        let metadata = match fs::metadata(&canonical) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) => {
                skipped.push(SkippedFile {
                    archive_path: candidate.archive_path,
                    reason: format!("cannot read metadata: {error}"),
                });
                continue;
            }
        };
        if exported_total >= MAX_BUNDLE_BYTES {
            skipped.push(SkippedFile {
                archive_path: candidate.archive_path,
                reason: "bundle size limit reached".into(),
            });
            continue;
        }
        let remaining = (MAX_BUNDLE_BYTES - exported_total) as u64;
        let read_limit = metadata.len().min(MAX_FILE_BYTES).min(remaining);
        let truncated = read_limit < metadata.len();
        let bytes = read_tail(&canonical, metadata.len(), read_limit)?;
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                skipped.push(SkippedFile {
                    archive_path: candidate.archive_path,
                    reason: "log is not UTF-8 text".into(),
                });
                continue;
            }
        };
        let sanitized = sanitize_text(&text, &replacements);
        let exported = sanitized.as_bytes();
        zip.start_file(&candidate.archive_path, options)?;
        zip.write_all(exported)?;
        exported_total = exported_total.saturating_add(exported.len());
        files.push(BundleFile {
            archive_path: candidate.archive_path,
            original_bytes: metadata.len(),
            exported_bytes: exported.len(),
            truncated,
            sha256: hex::encode(Sha256::digest(exported)),
        });
    }

    let manifest = BundleManifest {
        schema_version: 1,
        created_at: chrono::Utc::now().to_rfc3339(),
        limits: BundleLimits {
            max_file_bytes: MAX_FILE_BYTES,
            max_bundle_bytes: MAX_BUNDLE_BYTES,
        },
        files,
        skipped,
    };
    zip.start_file("manifest.json", options)?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    zip.finish()?;
    Ok(output)
}

fn collect_logs(root: &Path, prefix: &str, output: &mut Vec<Candidate>) -> std::io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    collect_logs_inner(root, root, prefix, output)
}

fn collect_logs_inner(
    root: &Path,
    directory: &Path,
    prefix: &str,
    output: &mut Vec<Candidate>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("support-bundles")
            {
                continue;
            }
            collect_logs_inner(root, &entry.path(), prefix, output)?;
        } else if metadata.is_file() && is_log_file(&entry.path()) {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(path.as_path());
            let archive_path = format!("{prefix}/{}", safe_archive_path(relative));
            output.push(Candidate { path, archive_path });
        }
    }
    Ok(())
}

fn is_log_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".log")
        || lower.contains(".log.")
        || lower.ends_with(".jsonl")
        || lower.contains(".jsonl.")
}

fn safe_archive_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().replace(['/', '\\'], "_")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn read_tail(path: &Path, file_len: u64, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    if limit < file_len {
        file.seek(SeekFrom::Start(file_len - limit))?;
    }
    let mut bytes = Vec::with_capacity(limit as usize);
    file.take(limit).read_to_end(&mut bytes)?;
    if limit < file_len {
        while bytes
            .first()
            .is_some_and(|byte| (*byte & 0b1100_0000) == 0b1000_0000)
        {
            bytes.remove(0);
        }
    }
    Ok(bytes)
}

fn path_replacements(data_root: &Path) -> Vec<(String, &'static str)> {
    let mut replacements = vec![(data_root.to_string_lossy().into_owned(), "<VELLUM_DATA>")];
    for key in ["USERPROFILE", "HOME"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() && !replacements.iter().any(|(known, _)| known == &value) {
                replacements.push((value, "<USER_HOME>"));
            }
        }
    }
    replacements.sort_by_key(|item| std::cmp::Reverse(item.0.len()));
    replacements
}

fn sanitize_text(text: &str, replacements: &[(String, &'static str)]) -> String {
    text.lines()
        .map(|line| {
            let mut sanitized = if let Ok(mut value) =
                serde_json::from_str::<serde_json::Value>(line)
            {
                sanitize_json(&mut value, replacements);
                serde_json::to_string(&value).unwrap_or_else(|_| "<REDACTED_INVALID_JSON>".into())
            } else {
                sanitize_plain(line, replacements)
            };
            if sanitized.len() > MAX_TEXT_LINE_BYTES {
                sanitized = redacted_long_value(&sanitized);
            }
            sanitized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_json(value: &mut serde_json::Value, replacements: &[(String, &'static str)]) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if secret_key(key) {
                    *value = serde_json::Value::String("<REDACTED>".into());
                } else if identifier_key(key) {
                    if let Some(identifier) = value.as_str() {
                        *value = serde_json::Value::String(anonymized_identifier(identifier));
                    }
                } else {
                    sanitize_json(value, replacements);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_json(value, replacements);
            }
        }
        serde_json::Value::String(text) => {
            *text = sanitize_plain(text, replacements);
            if text.len() > MAX_TEXT_LINE_BYTES {
                *text = redacted_long_value(text);
            }
        }
        _ => {}
    }
}

fn identifier_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "requestid"
            | "sessionid"
            | "threadid"
            | "callid"
            | "parentrequestid"
            | "childrequestid"
            | "parentthreadid"
            | "childthreadid"
    )
}

fn anonymized_identifier(identifier: &str) -> String {
    let digest = hex::encode(Sha256::digest(identifier.as_bytes()));
    format!("id:{}", &digest[..16])
}

fn secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    normalized.contains("authorization")
        || normalized.contains("apikey")
        || normalized.contains("accesstoken")
        || normalized.contains("refreshtoken")
        || normalized.contains("authtoken")
        || normalized == "idtoken"
        || normalized == "token"
        || normalized.contains("password")
        || normalized.contains("credential")
        || normalized == "secret"
        || normalized.ends_with("secret")
}

fn sanitize_plain(text: &str, replacements: &[(String, &'static str)]) -> String {
    let mut result = text.to_owned();
    for (needle, replacement) in replacements {
        result = replace_case_insensitive(&result, needle, replacement);
    }
    result = redact_bearer(&result);
    for marker in [
        "api_key=",
        "apikey=",
        "access_token=",
        "refresh_token=",
        "auth_token=",
        "id_token=",
        "token=",
        "password=",
        "api_key:",
        "apikey:",
        "authorization:",
        "access_token:",
        "refresh_token:",
        "auth_token:",
        "id_token:",
        "token:",
        "password:",
    ] {
        result = redact_assignment(&result, marker);
    }
    result
}

fn replace_case_insensitive(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    let lower_text = text.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(offset) = lower_text[cursor..].find(&lower_needle) {
        let start = cursor + offset;
        result.push_str(&text[cursor..start]);
        result.push_str(replacement);
        cursor = start + needle.len();
    }
    result.push_str(&text[cursor..]);
    result
}

fn redact_bearer(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("bearer ") {
        let start = cursor + offset;
        output.push_str(&text[cursor..start]);
        output.push_str("Bearer <REDACTED>");
        let token_start = start + "bearer ".len();
        cursor = text[token_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, ',' | '}' | ']')
            })
            .map(|end| token_start + end)
            .unwrap_or(text.len());
    }
    output.push_str(&text[cursor..]);
    output
}

fn redact_assignment(text: &str, marker: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let Some(start) = lower.find(marker) else {
        return text.to_owned();
    };
    let raw_value_start = start + marker.len();
    let value_start = text[raw_value_start..]
        .find(|character: char| !character.is_whitespace() && !matches!(character, '\'' | '"'))
        .map(|offset| raw_value_start + offset)
        .unwrap_or(text.len());
    let value_end = text[value_start..]
        .find(|character: char| {
            character.is_whitespace() || matches!(character, '\'' | '"' | ',' | ';' | '}' | ']')
        })
        .map(|end| value_start + end)
        .unwrap_or(text.len());
    format!(
        "{}{}<REDACTED>{}",
        &text[..start],
        &text[start..raw_value_start],
        &text[value_end..]
    )
}

fn redacted_long_value(text: &str) -> String {
    format!(
        "<REDACTED_LONG_VALUE bytes={} sha256={}>",
        text.len(),
        hex::encode(Sha256::digest(text.as_bytes()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_includes_only_redacted_logs_and_a_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let app = temp.path().join("app-logs");
        let downloads = temp.path().join("downloads");
        fs::create_dir_all(data.join("runtimes/enhanced/log")).unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(
            data.join("runtimes/enhanced/log/enhanced-events.jsonl"),
            format!(
                "{{\"event\":\"enhanced.context.trimmed\",\"apiKey\":\"secret-key\",\"path\":{:?}}}\n",
                data.join("private")
            ),
        )
        .unwrap();
        fs::write(
            app.join("vellum.log"),
            "Authorization: Bearer abc123\npassword=hunter2\nauth_token: another-secret\n",
        )
        .unwrap();
        fs::write(data.join("history.sqlite3"), b"conversation secret").unwrap();
        fs::write(data.join("settings.json"), b"api key secret").unwrap();

        let archive = export(
            &data,
            Some(&app),
            &downloads,
            Some((
                "{\"kind\":\"turn\",\"request_id\":\"private-request\"}\n".into(),
                false,
            )),
        )
        .unwrap();
        let mut zip = zip::ZipArchive::new(File::open(archive).unwrap()).unwrap();
        let names = (0..zip.len())
            .map(|index| zip.by_index(index).unwrap().name().to_owned())
            .collect::<Vec<_>>();
        assert!(names
            .iter()
            .any(|name| name.ends_with("enhanced-events.jsonl")));
        assert!(names.iter().any(|name| name == "app/vellum.log"));
        assert!(names
            .iter()
            .any(|name| name == "data/diagnostic-events.jsonl"));
        assert!(names.iter().any(|name| name == "manifest.json"));
        assert!(!names.iter().any(|name| name.contains("history.sqlite3")));
        assert!(!names.iter().any(|name| name.contains("settings.json")));

        let log_name = names
            .iter()
            .find(|name| name.ends_with("enhanced-events.jsonl"))
            .unwrap();
        let mut contents = String::new();
        zip.by_name(log_name)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert!(contents.contains("<REDACTED>"));
        assert!(contents.contains("<VELLUM_DATA>"));
        assert!(!contents.contains("secret-key"));

        contents.clear();
        zip.by_name("app/vellum.log")
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert!(contents.contains("Authorization:<REDACTED>"));
        assert!(contents.contains("password=<REDACTED>"));
        assert!(!contents.contains("abc123"));
        assert!(!contents.contains("hunter2"));
        assert!(!contents.contains("another-secret"));

        contents.clear();
        zip.by_name("data/diagnostic-events.jsonl")
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert!(contents.contains("id:"));
        assert!(!contents.contains("private-request"));
    }

    #[test]
    fn rotated_logs_are_allowed_but_similarly_named_data_is_not() {
        assert!(is_log_file(Path::new("enhanced-events.jsonl.3")));
        assert!(is_log_file(Path::new("vellum.log.1")));
        assert!(!is_log_file(Path::new("settings.json")));
        assert!(!is_log_file(Path::new("usage.sqlite3")));
    }
}
