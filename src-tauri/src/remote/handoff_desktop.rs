//! Codex Desktop links use journal-owned remote IDs and local Desktop metadata.
use super::*;

pub fn thread_link(state: &AppState, handoff_id: &str) -> AppResult<Option<String>> {
    let record = load(&state.data_root(), handoff_id)?;
    let Some(thread_id) = record
        .remote
        .as_ref()
        .and_then(|value| value["threadId"].as_str())
    else {
        return Ok(None);
    };
    let cached = state.remote().list_hosts()?;
    let host_id = super::super::discovery::desktop_host_id(&record.host_id, &cached);
    Ok(thread_uri(thread_id, host_id.as_deref()))
}

fn thread_uri(thread_id: &str, host_id: Option<&str>) -> Option<String> {
    let host_id = host_id.filter(|id| !id.trim().is_empty())?;
    if thread_id.trim().is_empty() {
        return None;
    }
    Some(format!(
        "codex://threads/{}?hostId={}",
        component(thread_id),
        component(host_id)
    ))
}

/// Match encodeURIComponent, including UTF-8 and reserved URI delimiters.
fn component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encodes_both_components_without_injecting_paths_or_query_parameters() {
        assert_eq!(thread_uri("remote/thread:?& #+", Some("remote-ssh-discovered:jetson-orin")),
            Some("codex://threads/remote%2Fthread%3A%3F%26%20%23%2B?hostId=remote-ssh-discovered%3Ajetson-orin".into()));
        assert_eq!(component("主機"), "%E4%B8%BB%E6%A9%9F");
        assert_eq!(component("a-_.!~*'()"), "a-_.!~*'()");
    }

    #[test]
    fn no_desktop_host_or_remote_thread_means_copy_id_fallback() {
        assert_eq!(thread_uri("remote", None), None);
        assert_eq!(thread_uri("remote", Some("")), None);
        assert_eq!(thread_uri("remote", Some("  ")), None);
        assert_eq!(thread_uri("", Some("host")), None);
    }
}
