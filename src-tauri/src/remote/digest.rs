//! Remote digest helpers that accept GNU `sha256sum` and BSD `shasum -a 256`.
//!
//! Paths with spaces are single-quoted. The generated remote script is data;
//! unit tests cover quoting and tool selection without opening SSH.

pub fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// POSIX snippet that prints the hex digest of `$1` using sha256sum or shasum.
pub fn remote_file_digest_snippet() -> &'static str {
    r#"digest_file() {
  _path=$1
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$_path" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$_path" | awk '{print $1}'
  else
    echo "RemoteDigestToolMissing: need sha256sum or shasum -a 256" >&2
    return 41
  fi
}"#
}

pub fn install_artifact_remote_script(binary_name: &str, digest: &str) -> String {
    let quoted_name = shell_single_quote(binary_name);
    let quoted_digest = shell_single_quote(digest);
    format!(
        r#"set -eu
umask 077
{digest_fn}
mkdir -p "$HOME/.local/bin" "$HOME/.local/state/vellum/bootstrap"
tmp="$HOME/.local/state/vellum/bootstrap/{name_unquoted}.tmp"
target="$HOME/.local/bin/"{quoted_name}
cat >"$tmp"
actual=$(digest_file "$tmp")
[ "$actual" = {quoted_digest} ] || {{ rm -f "$tmp"; exit 42; }}
chmod 0700 "$tmp"
mv -f "$tmp" "$HOME/.local/bin/"{quoted_name}
"#,
        digest_fn = remote_file_digest_snippet(),
        name_unquoted = binary_name,
        quoted_name = quoted_name,
        quoted_digest = quoted_digest,
    )
}

pub fn parse_digest_output(stdout: &str) -> Option<String> {
    let token = stdout
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    (token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_snippet_accepts_sha256sum_and_shasum() {
        let snippet = remote_file_digest_snippet();
        assert!(snippet.contains("sha256sum"));
        assert!(snippet.contains("shasum -a 256"));
        let script = install_artifact_remote_script("vellum-remote-agent", &"a".repeat(64));
        assert!(script.contains("shasum -a 256"));
        assert!(script.contains("chmod 0700"));
        assert!(script.contains("mv -f"));
    }

    #[test]
    fn paths_with_spaces_are_quoted_and_digest_parse_is_strict() {
        assert_eq!(
            shell_single_quote("/Users/josh huang/agent"),
            "'/Users/josh huang/agent'"
        );
        let script = install_artifact_remote_script("my agent", &"b".repeat(64));
        assert!(script.contains("'my agent'"));
        assert_eq!(parse_digest_output(&format!("{}\n", "c".repeat(64))), Some("c".repeat(64)));
        assert_eq!(
            parse_digest_output(&format!("{}  /tmp/file with spaces\n", "d".repeat(64))),
            Some("d".repeat(64))
        );
        assert!(parse_digest_output("not-a-digest").is_none());
    }
}
