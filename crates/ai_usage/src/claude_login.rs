//! Claude Code keeps one sign-in in the macOS keychain no matter which configuration directory it
//! runs with, so accounts are switched by swapping that sign-in with copies Luna keeps in its own
//! keychain items.

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CLAUDE_CODE_SERVICE: &str = "Claude Code-credentials";
const LUNA_SERVICE: &str = "Luna AI Accounts";

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

fn keychain_account_name() -> Result<String> {
    std::env::var("USER").context("USER isn't set")
}

/// The sign-in Claude Code currently uses, if it's signed in.
pub(crate) fn current_login() -> Result<Option<ClaudeLogin>> {
    let Some(credentials) = read_claude_code_credentials()? else {
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
            write_claude_code_credentials(&login.credentials).await?;
            write_oauth_account(login.oauth_account.clone())
        }
        None => {
            delete_claude_code_credentials().await?;
            write_oauth_account(None)
        }
    }
}

pub(crate) fn saved_login(account_id: &str) -> Result<Option<ClaudeLogin>> {
    let Some(saved) = keychain::read(LUNA_SERVICE, Some(account_id))? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&saved)?))
}

pub(crate) fn save_login(account_id: &str, login: &ClaudeLogin) -> Result<()> {
    keychain::write(LUNA_SERVICE, account_id, &serde_json::to_vec(login)?)
}

pub(crate) fn delete_saved_login(account_id: &str) -> Result<()> {
    keychain::delete(LUNA_SERVICE, account_id)
}

fn read_claude_code_credentials() -> Result<Option<String>> {
    let Some(credentials) = keychain::read(CLAUDE_CODE_SERVICE, None)? else {
        return Ok(None);
    };
    Ok(Some(String::from_utf8(credentials)?))
}

/// Uses the `security` tool, as Claude Code does, so the item stays readable by Claude Code
/// without a keychain prompt.
async fn write_claude_code_credentials(credentials: &str) -> Result<()> {
    let hex = credentials
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    run_security(&[
        "add-generic-password",
        "-U",
        "-a",
        &keychain_account_name()?,
        "-s",
        CLAUDE_CODE_SERVICE,
        "-X",
        &hex,
    ])
    .await
}

async fn delete_claude_code_credentials() -> Result<()> {
    if read_claude_code_credentials()?.is_none() {
        return Ok(());
    }
    run_security(&[
        "delete-generic-password",
        "-a",
        &keychain_account_name()?,
        "-s",
        CLAUDE_CODE_SERVICE,
    ])
    .await
}

async fn run_security(args: &[&str]) -> Result<()> {
    let output = util::command::new_command("/usr/bin/security")
        .args(args)
        .output()
        .await
        .context("couldn't run security")?;
    anyhow::ensure!(
        output.status.success(),
        "security {} failed: {}",
        args.first().copied().unwrap_or_default(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
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

#[cfg(target_os = "macos")]
mod keychain {
    use anyhow::Result;
    use security_framework::{
        item::{ItemClass, ItemSearchOptions, Limit, SearchResult},
        passwords::{delete_generic_password, set_generic_password},
    };

    /// `account` narrows the search; `None` matches any account under the service.
    pub fn read(service: &str, account: Option<&str>) -> Result<Option<Vec<u8>>> {
        let mut search = ItemSearchOptions::new();
        search
            .class(ItemClass::generic_password())
            .service(service)
            .load_data(true)
            .limit(Limit::Max(1));
        if let Some(account) = account {
            search.account(account);
        }
        let Ok(results) = search.search() else {
            return Ok(None);
        };
        Ok(results.into_iter().find_map(|result| match result {
            SearchResult::Data(data) => Some(data),
            _ => None,
        }))
    }

    pub fn write(service: &str, account: &str, data: &[u8]) -> Result<()> {
        set_generic_password(service, account, data)?;
        Ok(())
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        if read(service, Some(account))?.is_some() {
            delete_generic_password(service, account)?;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    use anyhow::{Result, bail};

    pub fn read(_service: &str, _account: Option<&str>) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    pub fn write(_service: &str, _account: &str, _data: &[u8]) -> Result<()> {
        bail!("switching Claude accounts is only supported on macOS")
    }

    pub fn delete(_service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_login_email_comes_from_the_account_details() {
        let login = ClaudeLogin {
            credentials: "{}".to_string(),
            oauth_account: Some(serde_json::json!({ "emailAddress": "someone@example.com" })),
        };
        assert_eq!(login.email().as_deref(), Some("someone@example.com"));
        let round_tripped =
            serde_json::from_slice::<ClaudeLogin>(&serde_json::to_vec(&login).unwrap()).unwrap();
        assert_eq!(
            round_tripped.email().as_deref(),
            Some("someone@example.com")
        );
    }
}
