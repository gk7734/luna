use collections::HashMap;
use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use gpui::{App, AppContext as _, Global};
use std::{
    cell::RefCell,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use util::ResultExt as _;

pub struct SymbolUsageDb(ThreadSafeConnection);

impl Domain for SymbolUsageDb {
    const NAME: &str = stringify!(SymbolUsageDb);
    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS symbol_usage(
            language TEXT NOT NULL,
            name TEXT NOT NULL,
            use_count INTEGER NOT NULL,
            last_used INTEGER NOT NULL,
            PRIMARY KEY(language, name)
        ) STRICT;
    )];
}

db::static_connection!(SymbolUsageDb, []);

/// Keeps the table bounded; the least recently used names are dropped first.
const MAX_STORED_USAGES: u32 = 20_000;

impl SymbolUsageDb {
    query! {
        fn all_usages() -> Result<Vec<(String, String, u32, i64)>> {
            SELECT language, name, use_count, last_used FROM symbol_usage
        }
    }

    query! {
        async fn record_usage(language: String, name: String, last_used: i64) -> Result<()> {
            INSERT INTO symbol_usage(language, name, use_count, last_used) VALUES ((?), (?), 1, (?))
            ON CONFLICT(language, name) DO UPDATE SET
                use_count = use_count + 1,
                last_used = excluded.last_used
        }
    }

    query! {
        async fn trim_usages(max_stored: u32) -> Result<()> {
            DELETE FROM symbol_usage WHERE rowid IN (
                SELECT rowid FROM symbol_usage ORDER BY last_used DESC LIMIT (-1) OFFSET (?)
            )
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Usage {
    count: u32,
    last_used: i64,
}

/// Ranking boosts for completions the user accepted before, keyed by language so a name picked in
/// one language doesn't reorder another's completions.
#[derive(Default)]
pub struct SymbolUsage {
    usages_by_language: HashMap<Arc<str>, HashMap<Arc<str>, Usage>>,
    scores_by_language: RefCell<HashMap<Arc<str>, UsageScores>>,
}

impl Global for SymbolUsage {}

#[derive(Clone, Debug, Default)]
pub struct UsageScores(Arc<HashMap<Arc<str>, u32>>);

impl UsageScores {
    pub fn new(scores: HashMap<Arc<str>, u32>) -> Self {
        Self(Arc::new(scores))
    }

    pub fn score(&self, name: &str) -> u32 {
        self.0.get(name).copied().unwrap_or(0)
    }
}

const RECENCY_HALF_LIFE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Caps the effect of repetition, so a name accepted hundreds of times doesn't permanently bury
/// one picked a few times recently.
const MAX_COUNTED_USES: u32 = 32;

fn score(usage: Usage, now: i64) -> u32 {
    let age = now.saturating_sub(usage.last_used).max(0) as f64;
    let recency = 0.5f64.powf(age / RECENCY_HALF_LIFE.as_secs_f64());
    let frequency = usage.count.min(MAX_COUNTED_USES) as f64;
    (1.0 + 1000.0 * frequency * recency) as u32
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

pub(crate) fn init(cx: &mut App) {
    cx.set_global(SymbolUsage::default());
    let db = SymbolUsageDb::global(cx);
    cx.spawn(async move |cx| {
        let rows = cx
            .background_executor()
            .spawn(async move { db.all_usages() })
            .await
            .log_err()
            .unwrap_or_default();
        cx.update(|cx| {
            let usage = cx.global_mut::<SymbolUsage>();
            for (language, name, count, last_used) in rows {
                usage
                    .usages_by_language
                    .entry(language.into())
                    .or_default()
                    .insert(name.into(), Usage { count, last_used });
            }
            usage.scores_by_language.get_mut().clear();
        });
    })
    .detach();
}

pub fn usage_scores(language: Option<&str>, cx: &App) -> UsageScores {
    let Some(usage) = cx.try_global::<SymbolUsage>() else {
        return UsageScores::default();
    };
    let language: Arc<str> = language.unwrap_or_default().into();
    if let Some(scores) = usage.scores_by_language.borrow().get(&language) {
        return scores.clone();
    }
    let now = now();
    let scores = UsageScores(Arc::new(
        usage
            .usages_by_language
            .get(&language)
            .map(|usages| {
                usages
                    .iter()
                    .map(|(name, usage)| (name.clone(), score(*usage, now)))
                    .collect()
            })
            .unwrap_or_default(),
    ));
    usage
        .scores_by_language
        .borrow_mut()
        .insert(language, scores.clone());
    scores
}

pub fn record_acceptance(language: Option<&str>, name: &str, cx: &mut App) {
    if name.is_empty() || !cx.has_global::<SymbolUsage>() {
        return;
    }
    let language: Arc<str> = language.unwrap_or_default().into();
    let now = now();
    let usage = cx.global_mut::<SymbolUsage>();
    let entry = usage
        .usages_by_language
        .entry(language.clone())
        .or_default()
        .entry(name.into())
        .or_insert(Usage {
            count: 0,
            last_used: now,
        });
    entry.count = entry.count.saturating_add(1);
    entry.last_used = now;
    usage.scores_by_language.get_mut().remove(&language);

    let db = SymbolUsageDb::global(cx);
    let name = name.to_string();
    cx.background_spawn(async move {
        db.record_usage(language.to_string(), name, now)
            .await
            .log_err();
        db.trim_usages(MAX_STORED_USAGES).await.log_err();
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recent_and_frequent_usages_score_higher() {
        let now = 1_000_000_000;
        let week = RECENCY_HALF_LIFE.as_secs() as i64;
        let once_now = score(
            Usage {
                count: 1,
                last_used: now,
            },
            now,
        );
        let often_now = score(
            Usage {
                count: 10,
                last_used: now,
            },
            now,
        );
        let once_last_week = score(
            Usage {
                count: 1,
                last_used: now - week,
            },
            now,
        );
        assert!(often_now > once_now);
        assert!(once_now > once_last_week);
        assert!(once_last_week > 0);
    }

    #[gpui::test]
    async fn test_usage_round_trips_through_db() {
        let db = SymbolUsageDb::open_test_db("test_usage_round_trips_through_db").await;
        db.record_usage("Rust".into(), "distance".into(), 10)
            .await
            .unwrap();
        db.record_usage("Rust".into(), "distance".into(), 20)
            .await
            .unwrap();
        assert_eq!(
            db.all_usages().unwrap(),
            vec![("Rust".to_string(), "distance".to_string(), 2, 20)]
        );
        db.record_usage("Rust".into(), "area".into(), 30)
            .await
            .unwrap();
        db.trim_usages(1).await.unwrap();
        assert_eq!(
            db.all_usages().unwrap(),
            vec![("Rust".to_string(), "area".to_string(), 1, 30)]
        );
    }
}
