use chrono::{DateTime, Utc};
use gpui::{
    Action, Anchor, AnyElement, App, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, px, relative,
};
use task::{SpawnInTerminal, TaskId};
use ui::{ButtonLike, ContextMenu, PopoverMenu, Tooltip, prelude::*};
use workspace::{
    HideStatusItem, StatusItemView, Toast, Workspace,
    item::ItemHandle,
    notifications::{NotificationId, NotifyTaskExt as _},
};

use crate::{
    Account, AccountUsage, AddClaudeAccount, AddCodexAccount, AiUsage, Provider, Refresh,
    UsageWindow,
};

/// Shows each AI CLI's active account usage in the status bar, with a menu to switch and add
/// accounts.
pub struct UsageIndicator {
    ai_usage: Option<Entity<AiUsage>>,
    _subscription: Option<Subscription>,
}

impl UsageIndicator {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let ai_usage = AiUsage::global(cx);
        let subscription = ai_usage
            .as_ref()
            .map(|ai_usage| cx.observe(ai_usage, |_, _, cx| cx.notify()));
        Self {
            ai_usage,
            _subscription: subscription,
        }
    }
}

pub(crate) fn register_workspace_actions(workspace: &mut Workspace) {
    workspace.register_action(|_, _: &Refresh, _, cx| {
        if let Some(ai_usage) = AiUsage::global(cx) {
            ai_usage.update(cx, |ai_usage, cx| ai_usage.refresh(cx));
        }
    });
    workspace.register_action(|workspace, _: &AddClaudeAccount, window, cx| {
        add_account(workspace, Provider::Claude, window, cx);
    });
    workspace.register_action(|workspace, _: &AddCodexAccount, window, cx| {
        add_account(workspace, Provider::Codex, window, cx);
    });
}

/// The new account becomes active, so the terminal opened for it runs the CLI against the new
/// account's own directory, where the CLI asks to sign in.
fn add_account(
    workspace: &mut Workspace,
    provider: Provider,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(ai_usage) = AiUsage::global(cx) else {
        return;
    };
    let add_account = ai_usage.update(cx, |ai_usage, cx| ai_usage.add_account(provider, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let account = add_account.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            let (command, args) = provider.sign_in_command();
            let label = format!("Sign in to {}", account.name);
            // The CLI starts signed out, so it goes straight to signing in, in the browser.
            workspace
                .spawn_in_terminal(
                    SpawnInTerminal {
                        id: TaskId(format!("ai-usage-sign-in-{}", account.id)),
                        full_label: label.clone(),
                        label: label.clone(),
                        command: Some(command.to_string()),
                        args: args.iter().map(|arg| arg.to_string()).collect(),
                        command_label: label,
                        use_new_terminal: true,
                        show_command: true,
                        ..SpawnInTerminal::default()
                    },
                    window,
                    cx,
                )
                .detach();
            struct AccountAdded;
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<AccountAdded>(),
                    format!(
                        "Sign in to {} in the terminal; its usage shows up once you're signed in.",
                        account.name
                    ),
                ),
                cx,
            );
        })
    })
    .detach_and_notify_err(workspace.weak_handle(), window, cx);
}

fn usage_color(used_percent: f32, cx: &App) -> gpui::Hsla {
    if used_percent >= 90. {
        cx.theme().status().error
    } else if used_percent >= 70. {
        cx.theme().status().warning
    } else {
        cx.theme().colors().text_accent
    }
}

fn usage_bar(used_percent: f32, width: gpui::Pixels, cx: &App) -> impl IntoElement {
    let fraction = (used_percent / 100.).clamp(0., 1.);
    div()
        .w(width)
        .h(px(4.))
        .rounded_full()
        .bg(cx.theme().colors().element_background)
        .child(
            div()
                .h_full()
                .w(relative(fraction))
                .rounded_full()
                .bg(usage_color(used_percent, cx)),
        )
}

fn time_until(resets_at: DateTime<Utc>) -> String {
    let remaining = resets_at - Utc::now();
    let minutes = remaining.num_minutes().max(0);
    let (days, hours) = (minutes / (24 * 60), minutes / 60 % 24);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {}m", minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

fn window_summary(window: &UsageWindow) -> String {
    let resets = window
        .resets_at
        .map(|resets_at| format!(" {}", time_until(resets_at)))
        .unwrap_or_default();
    format!("{} {:.0}%{resets}", window.label, window.used_percent)
}

fn render_compact(provider: Provider, usage: Option<&AccountUsage>, cx: &App) -> AnyElement {
    let icon = Icon::new(provider.icon()).size(IconSize::XSmall);
    let content = match usage {
        Some(AccountUsage::Loaded(report)) => {
            let first_window = report.windows.first();
            h_flex()
                .gap_1()
                .children(first_window.map(|window| usage_bar(window.used_percent, px(28.), cx)))
                .child(
                    Label::new(
                        report
                            .windows
                            .iter()
                            .take(2)
                            .map(window_summary)
                            .collect::<Vec<_>>()
                            .join(" · "),
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element()
        }
        Some(AccountUsage::Failed(_)) => Icon::new(IconName::Warning)
            .size(IconSize::XSmall)
            .color(Color::Warning)
            .into_any_element(),
        Some(AccountUsage::Loading) | None => Label::new("…")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    };
    h_flex()
        .gap_1()
        .child(icon.color(Color::Muted))
        .child(content)
        .into_any_element()
}

fn render_account_row(
    account: &Account,
    usage: Option<&AccountUsage>,
    is_active: bool,
    cx: &App,
) -> AnyElement {
    let header = h_flex()
        .gap_1p5()
        .child(
            Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(if is_active {
                    Color::Accent
                } else {
                    Color::Hidden
                }),
        )
        .child(Label::new(account.name.clone()))
        .children(match usage {
            Some(AccountUsage::Loaded(report)) => report.plan.clone().map(|plan| {
                Label::new(plan)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }),
            _ => None,
        });
    let details = match usage {
        Some(AccountUsage::Loaded(report)) => v_flex()
            .gap_1()
            .children(report.windows.iter().map(|window| {
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(48.))
                            .child(Label::new(window.label.clone()).size(LabelSize::Small)),
                    )
                    .child(usage_bar(window.used_percent, px(80.), cx))
                    .child(
                        Label::new(format!("{:.0}%", window.used_percent)).size(LabelSize::Small),
                    )
                    .children(window.resets_at.map(|resets_at| {
                        Label::new(format!("resets in {}", time_until(resets_at)))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                    }))
            }))
            .into_any_element(),
        Some(AccountUsage::Failed(error)) => Label::new(error.clone())
            .size(LabelSize::Small)
            .color(Color::Warning)
            .into_any_element(),
        Some(AccountUsage::Loading) | None => Label::new("Loading usage…")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    };
    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .child(div().pl_5().child(details))
        .into_any_element()
}

fn build_menu(ai_usage: Entity<AiUsage>, window: &mut Window, cx: &mut App) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |mut menu, _, cx| {
        // The active account can't be removed: its sign-in is the one the CLI uses.
        let removable_accounts = Provider::ALL
            .into_iter()
            .flat_map(|provider| {
                let state = ai_usage.read(cx);
                let active_id = state.active_account(provider).map(|account| account.id);
                state.accounts(provider).into_iter().filter(move |account| {
                    !account.id.ends_with("-default") && Some(&account.id) != active_id.as_ref()
                })
            })
            .collect::<Vec<_>>();
        for provider in Provider::ALL {
            let state = ai_usage.read(cx);
            let accounts = state.accounts(provider);
            if accounts.is_empty() {
                continue;
            }
            let active_id = state.active_account(provider).map(|account| account.id);
            menu = menu.header(provider.display_name());
            for account in accounts {
                let usage = ai_usage.read(cx).usage(&account.id).cloned();
                let is_active = active_id.as_ref() == Some(&account.id);
                let row_account = account.clone();
                let ai_usage = ai_usage.clone();
                menu = menu.custom_entry(
                    move |_, cx| render_account_row(&row_account, usage.as_ref(), is_active, cx),
                    move |_, cx| {
                        ai_usage.update(cx, |ai_usage, cx| ai_usage.set_active(&account, cx));
                    },
                );
            }
            menu = menu.separator();
        }
        menu = menu
            .action("Refresh", Refresh.boxed_clone())
            .action("Add Claude Account…", AddClaudeAccount.boxed_clone())
            .action("Add Codex Account…", AddCodexAccount.boxed_clone());
        if !removable_accounts.is_empty() {
            menu = menu.submenu("Remove Account", move |mut menu, _, _| {
                for account in &removable_accounts {
                    let ai_usage = ai_usage.clone();
                    let account_id = account.id.clone();
                    menu = menu.entry(account.name.clone(), None, move |_, cx| {
                        ai_usage
                            .update(cx, |ai_usage, cx| ai_usage.remove_account(&account_id, cx));
                    });
                }
                menu
            });
        }
        menu
    })
}

impl Render for UsageIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(ai_usage) = self.ai_usage.clone() else {
            return div().into_any_element();
        };
        let state = ai_usage.read(cx);
        let active_accounts = Provider::ALL
            .into_iter()
            .filter_map(|provider| state.active_account(provider))
            .collect::<Vec<_>>();
        if active_accounts.is_empty() {
            return div().into_any_element();
        }
        let trigger =
            ButtonLike::new("ai-usage")
                .child(h_flex().gap_3().children(
                    active_accounts.iter().map(|account| {
                        render_compact(account.provider, state.usage(&account.id), cx)
                    }),
                ))
                .tooltip(Tooltip::text("AI Account Usage"));
        PopoverMenu::new("ai-usage-menu")
            .trigger(trigger)
            .menu(move |window, cx| {
                ai_usage.update(cx, |ai_usage, cx| ai_usage.refresh_for_menu(cx));
                Some(build_menu(ai_usage.clone(), window, cx))
            })
            .anchor(Anchor::BottomLeft)
            .into_any_element()
    }
}

impl StatusItemView for UsageIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    /// Only shown once an AI CLI has been used on this machine.
    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
