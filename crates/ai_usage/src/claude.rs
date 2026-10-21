use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request, StatusCode};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{path::Path, sync::Arc};

use crate::{UsageReport, UsageWindow};

/// The endpoint Claude Code's `/usage` reads. It isn't a public API, so it may change.
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Windows Claude reports, in the order they're shown, with their short labels.
const KNOWN_WINDOWS: [(&str, &str); 4] = [
    ("five_hour", "5h"),
    ("seven_day", "wk"),
    ("seven_day_opus", "Opus"),
    ("seven_day_sonnet", "Sonnet"),
];

#[derive(Deserialize)]
struct StoredCredentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuthCredentials>,
}

#[derive(Deserialize)]
struct OAuthCredentials {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Milliseconds since the epoch.
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Claude Code keeps the default account's sign-in under a fixed keychain service, and suffixes it
/// with a hash of `CLAUDE_CONFIG_DIR` for other directories.
fn keychain_service(config_dir: Option<&Path>) -> String {
    match config_dir {
        None => KEYCHAIN_SERVICE.to_string(),
        Some(config_dir) => {
            let digest = Sha256::digest(config_dir.to_string_lossy().as_bytes());
            let hex = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            format!("{KEYCHAIN_SERVICE}-{}", &hex[..8])
        }
    }
}

#[cfg(target_os = "macos")]
fn read_keychain(service: &str) -> Option<Vec<u8>> {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};

    let results = ItemSearchOptions::new()
        .class(ItemClass::generic_password())
        .service(service)
        .load_data(true)
        .limit(Limit::Max(1))
        .search()
        .ok()?;
    results.into_iter().find_map(|result| match result {
        SearchResult::Data(data) => Some(data),
        _ => None,
    })
}

#[cfg(not(target_os = "macos"))]
fn read_keychain(_service: &str) -> Option<Vec<u8>> {
    None
}

fn load_credentials(config_dir: Option<&Path>) -> Result<OAuthCredentials> {
    let stored = match read_keychain(&keychain_service(config_dir)) {
        Some(stored) => stored,
        None => {
            let credentials_path = config_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| paths::home_dir().join(".claude"))
                .join(".credentials.json");
            std::fs::read(&credentials_path)
                .with_context(|| "not signed in to Claude Code with this account")?
        }
    };
    let stored = serde_json::from_slice::<StoredCredentials>(&stored)
        .context("couldn't read Claude Code's sign-in")?;
    stored
        .oauth
        .ok_or_else(|| anyhow!("Claude Code uses an API key, which has no usage limits to show"))
}

pub(crate) async fn fetch_usage(
    config_dir: Option<&Path>,
    http_client: Arc<dyn HttpClient>,
) -> Result<UsageReport> {
    let credentials = load_credentials(config_dir)?;
    // Refreshing the token here would rotate the one Claude Code stored and sign it out.
    if credentials
        .expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now().timestamp_millis())
    {
        bail!("sign-in expired; run claude once to renew it");
    }

    let request = Request::get(USAGE_URL)
        .header(
            "Authorization",
            format!("Bearer {}", credentials.access_token),
        )
        .header("anthropic-beta", OAUTH_BETA)
        .body(AsyncBody::empty())?;
    let mut response = http_client.send(request).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED => bail!("sign-in expired; run claude once to renew it"),
        status => bail!("usage request failed with {status}"),
    }
    let mut report = parse_usage(&body)?;
    report.plan = credentials.subscription_type.map(Into::into);
    Ok(report)
}

fn parse_usage(body: &str) -> Result<UsageReport> {
    let usage = serde_json::from_str::<serde_json::Value>(body)?;
    let object = usage
        .as_object()
        .ok_or_else(|| anyhow!("unexpected usage response"))?;
    let window = |key: &str, label: &str| -> Option<UsageWindow> {
        let window = object.get(key)?.as_object()?;
        Some(UsageWindow {
            label: label.to_string().into(),
            used_percent: window.get("utilization")?.as_f64()? as f32,
            resets_at: window
                .get("resets_at")
                .and_then(|resets_at| resets_at.as_str())
                .and_then(|resets_at| DateTime::parse_from_rfc3339(resets_at).ok())
                .map(|resets_at| resets_at.with_timezone(&Utc)),
        })
    };
    let windows = KNOWN_WINDOWS
        .iter()
        .filter_map(|(key, label)| window(key, label))
        .collect::<Vec<_>>();
    if windows.is_empty() {
        bail!("the usage response had no usage windows");
    }
    Ok(UsageReport {
        windows,
        plan: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_usage() {
        let report = parse_usage(
            r#"{
                "five_hour": { "utilization": 12.0, "resets_at": "2026-10-21T15:00:00+00:00" },
                "seven_day": { "utilization": 28.5, "resets_at": "2026-10-25T09:00:00Z" },
                "seven_day_opus": null,
                "seven_day_sonnet": { "utilization": 0, "resets_at": null }
            }"#,
        )
        .unwrap();
        let summary = report
            .windows
            .iter()
            .map(|window| {
                (
                    window.label.to_string(),
                    window.used_percent,
                    window.resets_at.map(|resets_at| resets_at.to_rfc3339()),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            vec![
                (
                    "5h".to_string(),
                    12.0,
                    Some("2026-10-21T15:00:00+00:00".to_string())
                ),
                (
                    "wk".to_string(),
                    28.5,
                    Some("2026-10-25T09:00:00+00:00".to_string())
                ),
                ("Sonnet".to_string(), 0.0, None),
            ]
        );
        assert!(parse_usage("{}").is_err());
    }

    #[test]
    fn test_reads_stored_credentials() {
        let stored = serde_json::from_str::<StoredCredentials>(
            r#"{ "claudeAiOauth": { "accessToken": "test-token", "expiresAt": 1, "subscriptionType": "max" } }"#,
        )
        .unwrap();
        let oauth = stored.oauth.unwrap();
        assert_eq!(oauth.access_token, "test-token");
        assert_eq!(oauth.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn test_keychain_service_is_suffixed_for_other_directories() {
        assert_eq!(keychain_service(None), "Claude Code-credentials");
        let service = keychain_service(Some(Path::new("/Users/someone/.claude-work")));
        assert!(service.starts_with("Claude Code-credentials-"));
        assert_eq!(service.len(), "Claude Code-credentials-".len() + 8);
    }
}
