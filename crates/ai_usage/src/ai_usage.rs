mod claude;
mod claude_login;
mod codex;
mod usage_indicator;

pub use usage_indicator::UsageIndicator;

use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{
    App, AppContext as _, Context, Entity, Global, SharedString, Task, TaskExt as _, actions,
};
use http_client::HttpClient;
use project::terminals::ExtraTerminalEnv;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
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

    /// The variable the CLI reads its configuration directory from, for CLIs whose accounts are
    /// kept apart by directory. Claude Code keeps one sign-in in the keychain whatever its
    /// directory, so its accounts are swapped instead.
    fn config_dir_env_var(self) -> Option<&'static str> {
        match self {
            Provider::Claude => None,
            Provider::Codex => Some("CODEX_HOME"),
        }
    }

    fn default_config_dir(self) -> PathBuf {
        match self {
            Provider::Claude => paths::home_dir().join(".claude"),
            Provider::Codex => paths::home_dir().join(".codex"),
        }
    }

    /// What to run in a terminal to sign in to a new account.
    pub fn sign_in_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Provider::Claude => ("claude", &[]),
            Provider::Codex => ("codex", &["login"]),
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
/// Opening the usage menu shows fresh numbers unless they're at most this old.
const MENU_REFRESH_AGE: Duration = Duration::from_secs(60);

pub struct AiUsage {
    added_accounts: Vec<Account>,
    active: HashMap<Provider, String>,
    usage: HashMap<String, AccountUsage>,
    http_client: Arc<dyn HttpClient>,
    refresh_task: Option<Task<()>>,
    sign_in_watch: Option<Task<()>>,
    last_refresh: Option<Instant>,
    _refresh_loop: Task<()>,
}

struct GlobalAiUsage(Entity<AiUsage>);

impl Global for GlobalAiUsage {}

pub fn init(cx: &mut App) {
    let mut stored = KeyValueStore::global(cx)
        .read_kvp(STORED_ACCOUNTS_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<StoredAccounts>(&json).log_err())
        .unwrap_or_default();
    migrate_directory_accounts(&mut stored, cx);
    let http_client = cx.http_client();
    let ai_usage = cx.new(|cx| {
        let this = AiUsage {
            added_accounts: stored.accounts,
            active: stored.active,
            usage: HashMap::default(),
            http_client,
            refresh_task: None,
            sign_in_watch: None,
            last_refresh: None,
            _refresh_loop: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(REFRESH_INTERVAL).await;
                    if this
                        .update(cx, |this: &mut AiUsage, cx| {
                            // Usage only matters while someone is looking at Luna.
                            if cx.active_window().is_some() {
                                this.refresh_if_older_than(REFRESH_INTERVAL, cx);
                            }
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }),
        };
        this.apply_terminal_env(cx);
        this
    });
    cx.set_global(GlobalAiUsage(ai_usage));
    // The first window opens after init, so usage loads as soon as there's one to show it in.
    cx.observe_new(|workspace: &mut workspace::Workspace, _, cx| {
        usage_indicator::register_workspace_actions(workspace);
        if let Some(ai_usage) = AiUsage::global(cx) {
            ai_usage.update(cx, |ai_usage, cx| {
                ai_usage.refresh_if_older_than(REFRESH_INTERVAL, cx)
            });
        }
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
        let previous = self.active_account(account.provider);
        if previous
            .as_ref()
            .is_some_and(|previous| previous.id == account.id)
        {
            return;
        }
        if account.provider != Provider::Claude {
            self.mark_active(account, cx);
            self.refresh(cx);
            return;
        }
        let account = account.clone();
        cx.spawn(async move |this, cx| {
            let switched = cx
                .background_spawn({
                    let account = account.clone();
                    async move { switch_claude_login(previous.as_ref(), &account).await }
                })
                .await;
            this.update(cx, |this, cx| match switched {
                Ok(()) => {
                    this.mark_active(&account, cx);
                    this.refresh(cx);
                }
                Err(error) => {
                    log::error!("Couldn't switch to {}: {error:#}", account.name);
                    this.usage.insert(
                        account.id.clone(),
                        AccountUsage::Failed(format!("couldn't switch: {error:#}").into()),
                    );
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn mark_active(&mut self, account: &Account, cx: &mut Context<Self>) {
        self.active.insert(account.provider, account.id.clone());
        self.apply_terminal_env(cx);
        self.save(cx);
        cx.notify();
    }

    /// Adds an account and makes it active with the CLI signed out, so the CLI asks to sign in
    /// the next time it runs: Claude Code's current sign-in is saved for its account and
    /// removed, and Codex runs with a directory of its own.
    pub fn add_account(
        &mut self,
        provider: Provider,
        cx: &mut Context<Self>,
    ) -> Task<Result<Account>> {
        let number = self
            .added_accounts
            .iter()
            .filter(|account| account.provider == provider)
            .count()
            + 2;
        let id = format!("{}-{number}", provider.id_prefix());
        let config_dir = provider
            .config_dir_env_var()
            .map(|_| paths::config_dir().join("ai-accounts").join(&id));
        let account = Account {
            id,
            provider,
            name: format!("{} {number}", provider.display_name()),
            config_dir,
        };
        let previous = self
            .active_account(provider)
            .unwrap_or_else(|| Account::default_for(provider));
        cx.spawn(async move |this, cx| {
            cx.background_spawn({
                let account = account.clone();
                async move {
                    match account.provider {
                        Provider::Claude => {
                            if let Some(login) = claude_login::current_login()? {
                                claude_login::save_login(&previous.id, &login)?;
                            }
                            claude_login::install_login(None).await
                        }
                        Provider::Codex => prepare_codex_home(&account.effective_config_dir()),
                    }
                }
            })
            .await?;
            this.update(cx, |this, cx| {
                this.added_accounts.push(account.clone());
                this.mark_active(&account, cx);
                this.watch_for_sign_in(account.clone(), cx);
            })?;
            Ok(account)
        })
    }

    /// Loads the new account's usage once its sign-in finishes in the terminal.
    fn watch_for_sign_in(&mut self, account: Account, cx: &mut Context<Self>) {
        const CHECK_INTERVAL: Duration = Duration::from_secs(3);
        const GIVE_UP_AFTER: Duration = Duration::from_secs(15 * 60);
        self.usage.insert(
            account.id.clone(),
            AccountUsage::Failed("waiting for sign-in".into()),
        );
        cx.notify();
        self.sign_in_watch = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            while started.elapsed() < GIVE_UP_AFTER {
                cx.background_executor().timer(CHECK_INTERVAL).await;
                let signed_in_as = cx
                    .background_spawn({
                        let account = account.clone();
                        async move { signed_in_name(&account) }
                    })
                    .await;
                let Some(name) = signed_in_as else {
                    continue;
                };
                this.update(cx, |this, cx| {
                    if let Some(added) = this
                        .added_accounts
                        .iter_mut()
                        .find(|added| added.id == account.id)
                        && let Some(name) = name
                    {
                        added.name = name;
                    }
                    this.save(cx);
                    this.refresh(cx);
                })
                .ok();
                return;
            }
        }));
    }

    pub fn remove_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        let removed = self
            .added_accounts
            .iter()
            .find(|account| account.id == account_id)
            .cloned();
        self.added_accounts
            .retain(|account| account.id != account_id);
        self.active.retain(|_, active_id| active_id != account_id);
        self.usage.remove(account_id);
        self.apply_terminal_env(cx);
        self.save(cx);
        cx.notify();
        if let Some(removed) = removed
            && removed.provider == Provider::Claude
        {
            cx.background_spawn(async move { claude_login::delete_saved_login(&removed.id) })
                .detach_and_log_err(cx);
        }
    }

    pub fn refresh_if_older_than(&mut self, max_age: Duration, cx: &mut Context<Self>) {
        let is_fresh = self
            .last_refresh
            .is_some_and(|last_refresh| last_refresh.elapsed() < max_age);
        if !is_fresh {
            self.refresh(cx);
        }
    }

    /// Fresh enough for the menu that's about to open.
    pub fn refresh_for_menu(&mut self, cx: &mut Context<Self>) {
        self.refresh_if_older_than(MENU_REFRESH_AGE, cx);
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.last_refresh = Some(Instant::now());
        let accounts = Provider::ALL
            .into_iter()
            .flat_map(|provider| {
                let active_id = self.active_account(provider).map(|account| account.id);
                self.accounts(provider).into_iter().map(move |account| {
                    let is_active = active_id.as_ref() == Some(&account.id);
                    (account, is_active)
                })
            })
            .collect::<Vec<_>>();
        for (account, _) in &accounts {
            self.usage
                .entry(account.id.clone())
                .or_insert(AccountUsage::Loading);
        }
        cx.notify();
        let http_client = self.http_client.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            for (account, is_active) in accounts {
                let http_client = http_client.clone();
                let result = cx
                    .background_spawn({
                        let account = account.clone();
                        async move { fetch_usage(&account, is_active, http_client).await }
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
                let env_var = provider.config_dir_env_var()?;
                let config_dir = self.active_account(provider)?.config_dir?;
                Some((
                    env_var.to_string(),
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

async fn fetch_usage(
    account: &Account,
    is_active: bool,
    http_client: Arc<dyn HttpClient>,
) -> Result<UsageReport> {
    match account.provider {
        Provider::Claude => {
            // Only the active account's sign-in is in Claude Code's keychain item; the others
            // are in the copies saved when switching away from them.
            let login = if is_active {
                claude_login::current_login()?
            } else {
                claude_login::saved_login(&account.id)?
            };
            let login = login.ok_or_else(|| anyhow!("not signed in"))?;
            claude::fetch_usage(&login.credentials, http_client).await
        }
        Provider::Codex => codex::fetch_usage(&account.effective_config_dir(), http_client).await,
    }
}

/// Saves the outgoing account's sign-in, which Claude Code may have renewed since it was
/// installed, before installing the incoming one's.
async fn switch_claude_login(previous: Option<&Account>, account: &Account) -> Result<()> {
    let incoming = claude_login::saved_login(&account.id)?.ok_or_else(|| {
        anyhow!(
            "no saved sign-in for {}; add the account again",
            account.name
        )
    })?;
    if let Some(previous) = previous
        && let Some(outgoing) = claude_login::current_login()?
    {
        claude_login::save_login(&previous.id, &outgoing)?;
    }
    claude_login::install_login(Some(&incoming)).await
}

/// `Some` once the account is signed in, with the name it should show when the sign-in tells it.
fn signed_in_name(account: &Account) -> Option<Option<String>> {
    match account.provider {
        Provider::Claude => {
            let login = claude_login::current_login().log_err().flatten()?;
            claude_login::save_login(&account.id, &login).log_err();
            Some(login.email())
        }
        Provider::Codex => account
            .effective_config_dir()
            .join("auth.json")
            .exists()
            .then_some(None),
    }
}

/// Earlier, accounts were kept apart by configuration directory alone. That never separated
/// Claude Code accounts, which all used the one keychain sign-in, so those are dropped; Codex
/// directories get the setting that keeps their sign-in in the directory.
fn migrate_directory_accounts(stored: &mut StoredAccounts, cx: &App) {
    let dropped = stored
        .accounts
        .iter()
        .filter(|account| account.provider == Provider::Claude && account.config_dir.is_some())
        .map(|account| account.id.clone())
        .collect::<Vec<_>>();
    stored
        .accounts
        .retain(|account| !dropped.contains(&account.id));
    stored
        .active
        .retain(|_, active_id| !dropped.contains(active_id));
    let codex_homes = stored
        .accounts
        .iter()
        .filter_map(|account| account.config_dir.clone())
        .filter(|config_dir| {
            !std::fs::read_to_string(config_dir.join("config.toml"))
                .is_ok_and(|config| config.contains("cli_auth_credentials_store"))
        })
        .collect::<Vec<_>>();
    if !codex_homes.is_empty() {
        cx.background_spawn(async move {
            for codex_home in codex_homes {
                prepare_codex_home(&codex_home).log_err();
            }
        })
        .detach();
    }
}

/// Codex keeps sign-ins in the keychain when it can, under the same name for every directory, so
/// a new account's directory stores its sign-in as a file there instead. The default account's
/// other settings carry over.
fn prepare_codex_home(config_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("couldn't create {}", config_dir.display()))?;
    let default_config =
        std::fs::read_to_string(Provider::Codex.default_config_dir().join("config.toml"))
            .unwrap_or_default();
    let other_settings = default_config
        .lines()
        .filter(|line| !line.trim_start().starts_with("cli_auth_credentials_store"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        config_dir.join("config.toml"),
        format!("cli_auth_credentials_store = \"file\"\n{other_settings}\n"),
    )
    .context("couldn't write the account's Codex settings")
}
