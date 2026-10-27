//! Claude Code keeps one sign-in in the macOS keychain no matter which configuration directory it
//! runs with, so accounts are switched by swapping that sign-in with copies Luna keeps in its own
//! keychain items.
//!
//! All keychain access goes through the `security` tool, as Claude Code's own does: the items it
//! creates already trust that tool, so reading and writing them never prompts.

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CLAUDE_CODE_SERVICE: &str = "Claude Code-credentials";
/// Items under the earlier "Luna AI Accounts" name were made through Security.framework and only
/// trust the Luna build that made them, so reading them prompts; they're left alone.
const LUNA_SERVICE: &str = "Luna AI Sign-ins";
/// `security`'s exit status when no item matches.
const ITEM_NOT_FOUND: i32 = 44;

/// A Claude Code sign-in: the credentials it keeps in the keychain, and the account details it
/// shows, which it keeps in `~/.claude.json`.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ClaudeLogin {
    pub credentials: String,
    pub oauth_account: Option<serde_json::Value>,
}

impl ClaudeLogin {
    pub fn email(&self) -> Option<String> {
        self.oauth_account
            .as_ref()?
            .get("emailAddress")?
            .as_str()
            .map(ToOwned::to_owned)
    }
}

fn claude_json_path() -> PathBuf {
    paths::home_dir().join(".claude.json")
}

/// The account name on Claude Code's keychain item, which writes must match for Claude Code to
/// find them. Only the item's attributes are read, which needs no permission.
async fn claude_code_account_name() -> Result<String> {
    if let Some(attributes) =
        run_security(&["find-generic-password", "-s", CLAUDE_CODE_SERVICE]).await?
        && let Some(account_name) = account_attribute(&attributes)
    {
        return Ok(account_name);
    }
    std::env::var("USER").context("USER isn't set")
}

/// Parses `"acct"<blob>="name"` from `security`'s description of an item.
fn account_attribute(attributes: &str) -> Option<String> {
    let line = attributes
        .lines()
        .find(|line| line.trim_start().starts_with("\"acct\"<blob>="))?;
    let value = line.split_once("<blob>=")?.1.trim();
    Some(value.strip_prefix('"')?.strip_suffix('"')?.to_string())
}

/// The sign-in Claude Code currently uses, if it's signed in.
pub(crate) async fn current_login() -> Result<Option<ClaudeLogin>> {
    let Some(credentials) = read_password(CLAUDE_CODE_SERVICE, None).await? else {
        return Ok(None);
    };
    Ok(Some(ClaudeLogin {
        credentials,
        oauth_account: read_oauth_account()?,
    }))
}

/// Makes Claude Code use `login`, or signs it out when there's none.
pub(crate) async fn install_login(login: Option<&ClaudeLogin>) -> Result<()> {
    match login {
        Some(login) => {
            let account_name = claude_code_account_name().await?;
            write_password(CLAUDE_CODE_SERVICE, &account_name, &login.credentials).await?;
            write_oauth_account(login.oauth_account.clone())
        }
        None => {
            delete_all_passwords(CLAUDE_CODE_SERVICE).await?;
            write_oauth_account(None)
        }
    }
}

pub(crate) async fn saved_login(account_id: &str) -> Result<Option<ClaudeLogin>> {
    let Some(saved) = read_password(LUNA_SERVICE, Some(account_id)).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_str(&saved)?))
}

pub(crate) async fn save_login(account_id: &str, login: &ClaudeLogin) -> Result<()> {
    write_password(LUNA_SERVICE, account_id, &serde_json::to_string(login)?).await
}

pub(crate) async fn delete_saved_login(account_id: &str) -> Result<()> {
    delete_password(LUNA_SERVICE, account_id).await
}

/// `account` narrows the search; `None` matches any account under the service.
async fn read_password(service: &str, account: Option<&str>) -> Result<Option<String>> {
    let mut args = vec!["find-generic-password", "-s", service];
    if let Some(account) = account {
        args.extend(["-a", account]);
    }
    args.push("-w");
    let Some(output) = run_security(&args).await? else {
        return Ok(None);
    };
    Ok(Some(decode_password_output(output.trim_end_matches('\n'))))
}

/// `security -w` prints a password as bare hex when it has bytes outside printable ASCII, such as
/// a Korean name in the account details. The passwords stored here are JSON objects, which are
/// never all hex digits, so hex output is always the encoded form.
fn decode_password_output(output: &str) -> String {
    let is_hex = !output.is_empty()
        && output.len().is_multiple_of(2)
        && output.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !is_hex {
        return output.to_string();
    }
    let bytes = (0..output.len())
        .step_by(2)
        .filter_map(|index| u8::from_str_radix(&output[index..index + 2], 16).ok())
        .collect::<Vec<_>>();
    String::from_utf8(bytes).unwrap_or_else(|_| output.to_string())
}

/// The password goes in as hex, as Claude Code passes it, so JSON quoting can't break it.
async fn write_password(service: &str, account: &str, password: &str) -> Result<()> {
    let hex = password
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    run_security(&[
        "add-generic-password",
        "-U",
        "-a",
        account,
        "-s",
        service,
        "-X",
        &hex,
    ])
    .await?
    .map(drop)
    .ok_or_else(|| anyhow!("couldn't save the {service} keychain item"))
}

async fn delete_password(service: &str, account: &str) -> Result<()> {
    run_security(&["delete-generic-password", "-a", account, "-s", service]).await?;
    Ok(())
}

/// Deletes every item under `service`, whatever its account name, so the CLI finds none.
async fn delete_all_passwords(service: &str) -> Result<()> {
    // Bounded in case an item can't be deleted and keeps matching.
    for _ in 0..16 {
        if run_security(&["delete-generic-password", "-s", service])
            .await?
            .is_none()
        {
            return Ok(());
        }
    }
    Err(anyhow!("couldn't remove the {service} keychain items"))
}

/// Returns the tool's output, or `None` when no keychain item matched.
async fn run_security(args: &[&str]) -> Result<Option<String>> {
    if !cfg!(target_os = "macos") {
        return Ok(None);
    }
    let output = util::command::new_command("/usr/bin/security")
        .args(args)
        .output()
        .await
        .context("couldn't run security")?;
    if output.status.code() == Some(ITEM_NOT_FOUND) {
        return Ok(None);
    }
    anyhow::ensure!(
        output.status.success(),
        "security {} failed: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(Some(String::from_utf8(output.stdout)?))
}

fn read_oauth_account() -> Result<Option<serde_json::Value>> {
    let Ok(contents) = std::fs::read_to_string(claude_json_path()) else {
        return Ok(None);
    };
    let config = serde_json::from_str::<serde_json::Value>(&contents)?;
    Ok(config.get("oauthAccount").cloned())
}

/// Replaces only the account details in `~/.claude.json`, keeping Claude Code's other settings
/// and their order.
fn write_oauth_account(oauth_account: Option<serde_json::Value>) -> Result<()> {
    let path = claude_json_path();
    let mut config = match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str::<serde_json::Value>(&contents)?,
        Err(_) if oauth_account.is_none() => return Ok(()),
        Err(_) => serde_json::json!({}),
    };
    let object = config
        .as_object_mut()
        .ok_or_else(|| anyhow!("~/.claude.json isn't a JSON object"))?;
    match oauth_account {
        Some(oauth_account) => {
            object.insert("oauthAccount".to_string(), oauth_account);
        }
        None => {
            object.shift_remove("oauthAccount");
        }
    }
    let temporary_path = path.with_extension("json.luna-tmp");
    std::fs::write(&temporary_path, serde_json::to_string_pretty(&config)?)?;
    std::fs::rename(&temporary_path, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reads_the_account_attribute() {
        let attributes = "keychain: \"/Users/someone/Library/Keychains/login.keychain-db\"\nattributes:\n    \"acct\"<blob>=\"someone\"\n    \"svce\"<blob>=\"Claude Code-credentials\"\n";
        assert_eq!(account_attribute(attributes).as_deref(), Some("someone"));
        assert_eq!(account_attribute("attributes:\n"), None);
    }

    #[test]
    fn test_decodes_passwords_security_prints_as_hex() {
        assert_eq!(
            decode_password_output("7b226e616d65223a22ec9db4eba684227d"),
            r#"{"name":"이름"}"#
        );
        assert_eq!(decode_password_output(r#"{"name":"abc"}"#), r#"{"name":"abc"}"#);
    }

    #[test]
    fn test_login_email_comes_from_the_account_details() {
        let login = ClaudeLogin {
            credentials: "{}".to_string(),
            oauth_account: Some(serde_json::json!({ "emailAddress": "someone@example.com" })),
        };
        assert_eq!(login.email().as_deref(), Some("someone@example.com"));
        let round_tripped =
            serde_json::from_str::<ClaudeLogin>(&serde_json::to_string(&login).unwrap()).unwrap();
        assert_eq!(
            round_tripped.email().as_deref(),
            Some("someone@example.com")
        );
    }
}
