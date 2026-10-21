mod claude;
mod codex;
mod usage_indicator;

pub use usage_indicator::UsageIndicator;

use anyhow::Result;
use chrono::{DateTime, Utc};
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Entity, Global, SharedString, Task, actions};
use http_client::HttpClient;
use project::terminals::ExtraTerminalEnv;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc, time::Duration};
use ui::IconName;
use util::ResultExt as _;

actions!(
    ai_usage,
    [
        /// Fetches the usage of every AI account again.
        Refresh,
        /// Adds a Claude Code account with its own configuration directory.
        AddClaudeAccount,
        /// Adds a Codex account with its own configuration directory.
        AddCodexAccount,
    ]
);

/// An AI coding CLI whose subscription usage Luna shows. Supporting another one means adding a
/// module that loads its sign-in and fetches its usage windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Provider::Claude => IconName::AiClaude,
            Provider::Codex => IconName::AiOpenAi,
        }
    }

    /// The variable the CLI reads its configuration directory from, which is how accounts are
    /// kept apart without copying sign-ins around.
    fn config_dir_env_var(self) -> &'static str {
        match self {
            Provider::Claude => "CLAUDE_CONFIG_DIR",
            Provider::Codex => "CODEX_HOME",
        }
    }

    fn default_config_dir(self) -> PathBuf {
        match self {
            Provider::Claude => paths::home_dir().join(".claude"),
            Provider::Codex => paths::home_dir().join(".codex"),
        }
    }

    /// What to run in a terminal to sign in to a new account.
    pub fn sign_in_command(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex login",
        }
    }

    fn id_prefix(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub provider: Provider,
    pub name: String,
    /// `None` for the CLI's own default directory, which needs no environment variable.
    pub config_dir: Option<PathBuf>,
}

impl Account {
    fn default_for(provider: Provider) -> Self {
        Self {
            id: format!("{}-default", provider.id_prefix()),
            provider,
            name: "Default".to_string(),
            config_dir: None,
        }
    }

    fn effective_config_dir(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(|| self.provider.default_config_dir())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub label: SharedString,
    pub used_percent: f32,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageReport {
    pub windows: Vec<UsageWindow>,
    pub plan: Option<SharedString>,
}

#[derive(Clone, Debug)]
pub enum AccountUsage {
    Loading,
    Loaded(UsageReport),
    Failed(SharedString),
}

#[derive(Default, Serialize, Deserialize)]
struct StoredAccounts {
    accounts: Vec<Account>,
    active: HashMap<Provider, String>,
}

const STORED_ACCOUNTS_KEY: &str = "ai_usage_accounts";
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub struct AiUsage {
    added_accounts: Vec<Account>,
    active: HashMap<Provider, String>,
    usage: HashMap<String, AccountUsage>,
    http_client: Arc<dyn HttpClient>,
    refresh_task: Option<Task<()>>,
    _refresh_loop: Task<()>,
}

struct GlobalAiUsage(Entity<AiUsage>);

impl Global for GlobalAiUsage {}

pub fn init(cx: &mut App) {
    let stored = KeyValueStore::global(cx)
        .read_kvp(STORED_ACCOUNTS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<StoredAccounts>(&json).log_err())
        .unwrap_or_default();
    let http_client = cx.http_client();
    let ai_usage = cx.new(|cx| {
        let this = AiUsage {
            added_accounts: stored.accounts,
            active: stored.active,
            usage: HashMap::default(),
            http_client,
            refresh_task: None,
            _refresh_loop: cx.spawn(async move |this, cx| {
                loop {
                    if this
                        .update(cx, |this: &mut AiUsage, cx| {
                            // Usage only matters while someone is looking at Luna.
                            if cx.active_window().is_some() {
                                this.refresh(cx);
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                    cx.background_executor().timer(REFRESH_INTERVAL).await;
                }
            }),
        };
        this.apply_terminal_env(cx);
        this
    });
    cx.set_global(GlobalAiUsage(ai_usage));
    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        usage_indicator::register_workspace_actions(workspace);
    })
    .detach();
}

impl AiUsage {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAiUsage>()
            .map(|ai_usage| ai_usage.0.clone())
    }

    /// The CLI's default account, when it has been used on this machine, followed by the added
    /// ones.
    pub fn accounts(&self, provider: Provider) -> Vec<Account> {
        let default_account = Account::default_for(provider);
        let has_default = default_account.effective_config_dir().exists();
        has_default
            .then_some(default_account)
            .into_iter()
            .chain(
                self.added_accounts
                    .iter()
                    .filter(|account| account.provider == provider)
                    .cloned(),
            )
            .collect()
    }

    pub fn active_account(&self, provider: Provider) -> Option<Account> {
        let accounts = self.accounts(provider);
        let active_id = self.active.get(&provider);
        accounts
            .iter()
            .find(|account| Some(&account.id) == active_id)
            .or_else(|| accounts.first())
            .cloned()
    }

    pub fn usage(&self, account_id: &str) -> Option<&AccountUsage> {
        self.usage.get(account_id)
    }

    pub fn set_active(&mut self, account: &Account, cx: &mut Context<Self>) {
        self.active.insert(account.provider, account.id.clone());
        self.apply_terminal_env(cx);
        self.save(cx);
        cx.notify();
    }

    /// Adds an account whose CLI configuration lives in its own directory under Luna's, and makes
    /// it active so the next terminal signs in to it.
    pub fn add_account(&mut self, provider: Provider, cx: &mut Context<Self>) -> Account {
        let number = self
            .added_accounts
            .iter()
            .filter(|account| account.provider == provider)
            .count()
            + 2;
        let id = format!("{}-{number}", provider.id_prefix());
        let config_dir = paths::config_dir().join("ai-accounts").join(&id);
        if let Err(error) = std::fs::create_dir_all(&config_dir) {
            log::error!("Couldn't create {config_dir:?}: {error}");
        }
        let account = Account {
            id,
            provider,
            name: format!("{} {number}", provider.display_name()),
            config_dir: Some(config_dir),
        };
        self.added_accounts.push(account.clone());
        self.set_active(&account, cx);
        account
    }

    pub fn remove_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        self.added_accounts
            .retain(|account| account.id != account_id);
        self.active.retain(|_, active_id| active_id != account_id);
        self.usage.remove(account_id);
        self.apply_terminal_env(cx);
        self.save(cx);
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let accounts = Provider::ALL
            .into_iter()
            .flat_map(|provider| self.accounts(provider))
            .collect::<Vec<_>>();
        for account in &accounts {
            self.usage
                .entry(account.id.clone())
                .or_insert(AccountUsage::Loading);
        }
        cx.notify();
        let http_client = self.http_client.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            for account in accounts {
                let http_client = http_client.clone();
                let result = cx
                    .background_spawn({
                        let account = account.clone();
                        async move { fetch_usage(&account, http_client).await }
                    })
                    .await;
                let usage = match result {
                    Ok(report) => AccountUsage::Loaded(report),
                    Err(error) => {
                        log::info!("Couldn't load {} usage: {error:#}", account.name);
                        AccountUsage::Failed(format!("{error:#}").into())
                    }
                };
                if this
                    .update(cx, |this, cx| {
                        this.usage.insert(account.id.clone(), usage);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    /// New terminals run each CLI with the active account's configuration directory.
    fn apply_terminal_env(&self, cx: &mut Context<Self>) {
        let env = Provider::ALL
            .into_iter()
            .filter_map(|provider| {
                let config_dir = self.active_account(provider)?.config_dir?;
                Some((
                    provider.config_dir_env_var().to_string(),
                    config_dir.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        cx.set_global(ExtraTerminalEnv(env));
    }

    fn save(&self, cx: &mut Context<Self>) {
        let stored = StoredAccounts {
            accounts: self.added_accounts.clone(),
            active: self.active.clone(),
        };
        let Some(json) = serde_json::to_string(&stored).log_err() else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        db::write_and_log(cx, move || async move {
            kvp.write_kvp(STORED_ACCOUNTS_KEY.to_string(), json).await
        });
    }
}

async fn fetch_usage(account: &Account, http_client: Arc<dyn HttpClient>) -> Result<UsageReport> {
    match account.provider {
        Provider::Claude => claude::fetch_usage(account.config_dir.as_deref(), http_client).await,
        Provider::Codex => codex::fetch_usage(&account.effective_config_dir(), http_client).await,
    }
}
