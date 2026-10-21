use anyhow::{Context as _, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request, StatusCode};
use serde::Deserialize;
use std::{path::Path, sync::Arc};

use crate::{RateLimited, UsageReport, UsageWindow};

/// The endpoint the Codex CLI reads its rate limits from. It isn't a public API, so it may change.
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Deserialize)]
struct StoredAuth {
    tokens: Option<Tokens>,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
}

#[derive(Deserialize)]
struct RateLimit {
    primary_window: Option<RateLimitWindow>,
    secondary_window: Option<RateLimitWindow>,
}

#[derive(Deserialize)]
struct RateLimitWindow {
    used_percent: f64,
    limit_window_seconds: Option<i64>,
    /// Seconds since the epoch.
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
}

fn load_tokens(config_dir: &Path) -> Result<Tokens> {
    let auth = std::fs::read(config_dir.join("auth.json"))
        .context("not signed in to Codex with this account")?;
    let auth =
        serde_json::from_slice::<StoredAuth>(&auth).context("couldn't read Codex's sign-in")?;
    auth.tokens.ok_or_else(|| {
        anyhow!("Codex uses an API key; usage limits only apply to ChatGPT sign-ins")
    })
}

pub(crate) async fn fetch_usage(
    config_dir: &Path,
    http_client: Arc<dyn HttpClient>,
) -> Result<UsageReport> {
    let tokens = load_tokens(config_dir)?;
    let mut request =
        Request::get(USAGE_URL).header("Authorization", format!("Bearer {}", tokens.access_token));
    if let Some(account_id) = &tokens.account_id {
        request = request.header("ChatGPT-Account-Id", account_id);
    }
    let mut response = http_client.send(request.body(AsyncBody::empty())?).await?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    match response.status() {
        StatusCode::OK => {}
        // Refreshing the token here would rotate the one Codex stored and sign it out.
        StatusCode::UNAUTHORIZED => bail!("sign-in expired; run codex once to renew it"),
        StatusCode::TOO_MANY_REQUESTS => return Err(RateLimited.into()),
        status => bail!("usage request failed with {status}"),
    }
    parse_usage(&body, Utc::now())
}

fn parse_usage(body: &str, now: DateTime<Utc>) -> Result<UsageReport> {
    let usage = serde_json::from_str::<UsageResponse>(body)?;
    let rate_limit = usage
        .rate_limit
        .ok_or_else(|| anyhow!("the usage response had no rate limits"))?;
    let windows = [rate_limit.primary_window, rate_limit.secondary_window]
        .into_iter()
        .flatten()
        .map(|window| UsageWindow {
            label: window_label(window.limit_window_seconds).into(),
            used_percent: window.used_percent as f32,
            resets_at: window
                .reset_at
                .and_then(|reset_at| DateTime::from_timestamp(reset_at, 0))
                .or_else(|| {
                    window
                        .reset_after_seconds
                        .map(|seconds| now + chrono::Duration::seconds(seconds))
                }),
        })
        .collect::<Vec<_>>();
    if windows.is_empty() {
        bail!("the usage response had no usage windows");
    }
    Ok(UsageReport {
        windows,
        plan: usage.plan_type.map(Into::into),
    })
}

fn window_label(window_seconds: Option<i64>) -> String {
    const HOUR: i64 = 60 * 60;
    const DAY: i64 = 24 * HOUR;
    match window_seconds {
        Some(seconds) if seconds >= 7 * DAY => "wk".to_string(),
        Some(seconds) if seconds >= DAY => format!("{}d", seconds / DAY),
        Some(seconds) if seconds > 0 => format!("{}h", (seconds + HOUR / 2) / HOUR),
        _ => "limit".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_usage() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let report = parse_usage(
            r#"{
                "plan_type": "plus",
                "rate_limit": {
                    "allowed": true,
                    "primary_window": { "used_percent": 50, "limit_window_seconds": 18000, "reset_after_seconds": 3600 },
                    "secondary_window": { "used_percent": 12.5, "limit_window_seconds": 604800, "reset_at": 1800500000 }
                }
            }"#,
            now,
        )
        .unwrap();
        assert_eq!(report.plan.as_deref(), Some("plus"));
        assert_eq!(
            report.windows,
            vec![
                UsageWindow {
                    label: "5h".into(),
                    used_percent: 50.0,
                    resets_at: Some(now + chrono::Duration::hours(1)),
                },
                UsageWindow {
                    label: "wk".into(),
                    used_percent: 12.5,
                    resets_at: DateTime::from_timestamp(1_800_500_000, 0),
                },
            ]
        );
        assert!(parse_usage(r#"{ "plan_type": "plus" }"#, now).is_err());
    }

    #[test]
    fn test_reads_stored_tokens() {
        let auth = serde_json::from_str::<StoredAuth>(
            r#"{ "OPENAI_API_KEY": null, "tokens": { "id_token": "x", "access_token": "test-token", "refresh_token": "y", "account_id": "acct" } }"#,
        )
        .unwrap();
        let tokens = auth.tokens.unwrap();
        assert_eq!(tokens.access_token, "test-token");
        assert_eq!(tokens.account_id.as_deref(), Some("acct"));
    }
}
