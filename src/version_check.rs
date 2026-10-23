// Copyright (C) 2026 Christos A. Daggas
// SPDX-License-Identifier: MIT

//! One startup GitHub release check, with silent failure and bounded input.

use serde::Deserialize;
use tracing::debug;

const GITHUB_OWNER: &str = "christosdaggas";
const GITHUB_REPO: &str = "speech-to-text";
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub latest_version: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

pub async fn check_for_update(current_version: &str) -> Option<UpdateInfo> {
    let url = format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .https_only(true)
        .user_agent(format!("SpeechToText/{current_version}"))
        .build()
        .ok()?;

    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return None;
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if !append_chunk(&mut body, &chunk) {
            debug!("Ignoring oversized update response");
            return None;
        }
    }
    update_from_response(&body, current_version)
}

fn append_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> bool {
    if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(body.len()) {
        return false;
    }
    body.extend_from_slice(chunk);
    true
}

fn update_from_response(body: &[u8], current_version: &str) -> Option<UpdateInfo> {
    if body.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    let release: GitHubRelease = serde_json::from_slice(body).ok()?;
    if release.draft || release.prerelease {
        return None;
    }
    let latest = release
        .tag_name
        .strip_prefix(['v', 'V'])
        .unwrap_or(&release.tag_name);
    if is_newer(latest, current_version) {
        Some(UpdateInfo {
            latest_version: latest.to_owned(),
        })
    } else {
        None
    }
}

fn stable_version(version: &str) -> Option<[u64; 3]> {
    if version.len() > 64 {
        return None;
    }
    let mut parts = version.split('.');
    let mut numbers = [0; 3];
    for number in &mut numbers {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *number = part.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

fn is_newer(remote: &str, local: &str) -> bool {
    match (stable_version(remote), stable_version(local)) {
        (Some(remote), Some(local)) => remote > local,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_all_three_numeric_components() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.9", "1.0.0"));
    }

    #[test]
    fn rejects_malformed_or_non_release_versions() {
        for remote in [
            "2",
            "2.0",
            "2.0.0.1",
            "2.0.0-beta",
            "2.0.0+build",
            "2.bad.0",
            "02.0.0",
            "2.0.0<script>",
            "18446744073709551616.0.0",
            "",
        ] {
            assert!(!is_newer(remote, "1.0.0"), "accepted {remote}");
        }
        assert!(!is_newer("2.0.0", "invalid"));
    }

    #[test]
    fn parses_github_releases_without_trusting_a_remote_url() {
        for tag in ["v1.0.1", "V1.0.1", "1.0.1"] {
            let body = serde_json::json!({"tag_name": tag, "html_url": "file:///ignored"});
            let info = update_from_response(body.to_string().as_bytes(), "1.0.0").unwrap();
            assert_eq!(info.latest_version, "1.0.1");
        }
        for body in [
            r#"{"tag_name":"1.0.0"}"#,
            r#"{"tag_name":"0.9.0"}"#,
            r#"{"tag_name":"v2.0.0","draft":true}"#,
            r#"{"tag_name":"v2.0.0","prerelease":true}"#,
            r#"{"tag_name":"v2.0.0-beta"}"#,
            r#"{"tag_name":"vv2.0.0"}"#,
            r#"{"message":"Not Found"}"#,
            "not json",
        ] {
            assert!(update_from_response(body.as_bytes(), "1.0.0").is_none());
        }
    }

    #[test]
    fn streamed_response_limit_rejects_overflow_without_appending() {
        let mut body = Vec::new();
        assert!(append_chunk(&mut body, &vec![b' '; MAX_RESPONSE_BYTES - 1]));
        assert!(append_chunk(&mut body, b" "));
        assert!(!append_chunk(&mut body, b"x"));
        assert_eq!(body.len(), MAX_RESPONSE_BYTES);
        assert!(!append_chunk(
            &mut Vec::new(),
            &vec![0; MAX_RESPONSE_BYTES + 1]
        ));
        assert!(update_from_response(&vec![0; MAX_RESPONSE_BYTES + 1], "1.0.0").is_none());
    }
}
