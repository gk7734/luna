mod cache;
mod extract;
mod usage;

pub use extract::{IndexedSymbol, SymbolKind, TodoItem};
pub use usage::{UsageScores, record_acceptance, usage_scores};

use collections::{HashMap, HashSet};
use futures::{StreamExt as _, channel::mpsc};
use fuzzy::StringMatchCandidate;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, Subscription, Task, WeakEntity,
};
use language::LanguageRegistry;
use parking_lot::Mutex;
use project::{Fs, Project, WorktreeId, WorktreeSettings};
use settings::{Settings as _, SettingsStore};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use util::{ResultExt as _, rel_path::RelPath};

/// Larger files are mostly generated or vendored, and parsing them would dominate indexing time.
const MAX_INDEXED_FILE_SIZE: u64 = 512 * 1024;
const INDEX_BATCH_SIZE: usize = 64;

pub fn init(cx: &mut App) {
    usage::init(cx);
    let index = cx.new(|cx| {
        // Files changed while indexing was paused never produced events, so resuming re-queues
        // every worktree.
        let mut indexing = WorktreeSettings::get_global(cx).indexing;
        cx.observe_global::<SettingsStore>(move |this: &mut SymbolIndex, cx| {
            let new_indexing = WorktreeSettings::get_global(cx).indexing;
            if std::mem::replace(&mut indexing, new_indexing) != new_indexing && new_indexing {
                this.requeue_all(cx);
            }
        })
        .detach();
        SymbolIndex::default()
    });
    cx.set_global(GlobalSymbolIndex(index.clone()));
    cx.observe_new(move |project: &mut Project, _, cx| {
        // Remote projects' files aren't on the local filesystem.
        if !project.is_local() {
            return;
        }
        let project = cx.entity();
        let index = index.clone();
        // The project is still being updated here, so it can't be read until later.
        cx.defer(move |cx| {
            index.update(cx, |index, cx| index.add_project(&project, cx));
        });
    })
    .detach();
}

struct GlobalSymbolIndex(Entity<SymbolIndex>);

impl Global for GlobalSymbolIndex {}

#[derive(Default)]
pub struct SymbolIndex {
    projects: HashMap<EntityId, ProjectIndex>,
    /// Names like `new` repeat across thousands of files; they're stored once.
    interned_names: HashSet<Arc<str>>,
}

struct ProjectIndex {
    project: WeakEntity<Project>,
    files: HashMap<(WorktreeId, Arc<RelPath>), IndexedFile>,
    generation: u64,
    name_table: Arc<Mutex<Option<(u64, Arc<NameTable>)>>>,
    jobs: mpsc::UnboundedSender<IndexJob>,
    _subscriptions: Vec<Subscription>,
    _indexer: Task<()>,
}

struct IndexedFile {
    symbols: Arc<[IndexedSymbol]>,
    todos: Arc<[TodoItem]>,
}

#[derive(Clone, Debug)]
pub struct FileTodos {
    pub worktree_id: WorktreeId,
    pub path: Arc<RelPath>,
    pub todos: Arc<[TodoItem]>,
}

struct IndexJob {
    worktree_id: WorktreeId,
    path: Arc<RelPath>,
    abs_path: PathBuf,
    /// Nanoseconds since the epoch, when known, to reuse the cached result for the same file.
    mtime: Option<i64>,
    size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolCandidate {
    pub name: Arc<str>,
    pub kind: SymbolKind,
    pub path: Arc<RelPath>,
}

struct NameTable {
    match_candidates: Vec<StringMatchCandidate>,
    symbols: Vec<SymbolCandidate>,
}

impl NameTable {
    fn build(files: &[(Arc<RelPath>, Arc<[IndexedSymbol]>)]) -> Self {
        let mut seen = HashSet::default();
        let mut match_candidates = Vec::new();
        let mut symbols = Vec::new();
        for (path, file_symbols) in files {
            for symbol in file_symbols.iter() {
                if !seen.insert(symbol.name.clone()) {
                    continue;
                }
                match_candidates.push(StringMatchCandidate::new(symbols.len(), &symbol.name));
                symbols.push(SymbolCandidate {
                    name: symbol.name.clone(),
                    kind: symbol.kind,
                    path: path.clone(),
                });
            }
        }
        Self {
            match_candidates,
            symbols,
        }
    }
}

impl SymbolIndex {
    fn add_project(&mut self, project: &Entity<Project>, cx: &mut Context<Self>) {
        let project_id = project.entity_id();
        if self.projects.contains_key(&project_id) {
            return;
        }
        let (jobs_tx, jobs_rx) = mpsc::unbounded();
        let languages = project.read(cx).languages().clone();
        let fs = project.read(cx).fs().clone();
        let indexer = cx.spawn(async move |this, cx| {
            run_indexer(this, project_id, languages, fs, jobs_rx, cx).await
        });
        let subscriptions = vec![
            cx.subscribe(project, Self::on_project_event),
            cx.observe_release(project, move |this, _, _| {
                this.projects.remove(&project_id);
            }),
        ];
        self.projects.insert(
            project_id,
            ProjectIndex {
                project: project.downgrade(),
                files: HashMap::default(),
                generation: 0,
                name_table: Arc::default(),
                jobs: jobs_tx,
                _subscriptions: subscriptions,
                _indexer: indexer,
            },
        );

        let worktree_ids = project
            .read(cx)
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>();
        for worktree_id in worktree_ids {
            self.queue_worktree(project, worktree_id, None, cx);
        }
    }

    fn requeue_all(&mut self, cx: &mut Context<Self>) {
        let projects = self
            .projects
            .values()
            .filter_map(|project_index| project_index.project.upgrade())
            .collect::<Vec<_>>();
        for project in projects {
            let worktree_ids = project
                .read(cx)
                .worktrees(cx)
                .map(|worktree| worktree.read(cx).id())
                .collect::<Vec<_>>();
            for worktree_id in worktree_ids {
                self.queue_worktree(&project, worktree_id, None, cx);
            }
        }
    }

    fn on_project_event(
        &mut self,
        project: Entity<Project>,
        event: &project::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            project::Event::WorktreeRemoved(worktree_id) => {
                if let Some(project_index) = self.projects.get_mut(&project.entity_id()) {
                    project_index
                        .files
                        .retain(|(file_worktree_id, _), _| file_worktree_id != worktree_id);
                    project_index.generation += 1;
                    cx.notify();
                }
            }
            project::Event::WorktreeUpdatedEntries(worktree_id, changes) => {
                let changed_paths = changes
                    .iter()
                    .map(|(path, _, _)| path.clone())
                    .collect::<Vec<_>>();
                self.queue_worktree(&project, *worktree_id, Some(changed_paths), cx);
            }
            _ => {}
        }
    }

    /// Queues the given paths, or every file in the worktree when `paths` is `None`. Paths that no
    /// longer point at an indexable file are dropped from the index.
    fn queue_worktree(
        &mut self,
        project: &Entity<Project>,
        worktree_id: WorktreeId,
        paths: Option<Vec<Arc<RelPath>>>,
        cx: &mut Context<Self>,
    ) {
        // The paused scanner sends no updates; skip the initial pass too so nothing runs.
        if !WorktreeSettings::get_global(cx).indexing {
            return;
        }
        let Some(project_index) = self.projects.get_mut(&project.entity_id()) else {
            return;
        };
        let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
            return;
        };
        let snapshot = worktree.read(cx).snapshot();
        // Events name removed files, whose cached results can go; a full pass only names files.
        let is_update = paths.is_some();
        let paths = paths.unwrap_or_else(|| {
            snapshot
                .files(false, 0)
                .map(|entry| entry.path.clone())
                .collect()
        });
        let mut removed_any = false;
        let mut uncached_paths = Vec::new();
        for path in paths {
            let indexable_entry = snapshot.entry_for_path(&path).filter(|entry| {
                entry.is_file()
                    && !entry.is_ignored
                    && !entry.is_external
                    && !entry.is_private
                    && entry.size <= MAX_INDEXED_FILE_SIZE
            });
            if let Some(entry) = indexable_entry {
                let abs_path = snapshot.absolutize(&path);
                let mtime = entry
                    .mtime
                    .and_then(|mtime| mtime.to_seconds_and_nanos_for_persistence())
                    .map(|(seconds, nanos)| seconds as i64 * 1_000_000_000 + nanos as i64);
                project_index
                    .jobs
                    .unbounded_send(IndexJob {
                        worktree_id,
                        path,
                        abs_path,
                        mtime,
                        size: entry.size,
                    })
                    .log_err();
            } else {
                if is_update {
                    uncached_paths.push(snapshot.absolutize(&path).to_string_lossy().into_owned());
                }
                if project_index.files.remove(&(worktree_id, path)).is_some() {
                    removed_any = true;
                }
            }
        }
        if !uncached_paths.is_empty() {
            let db = cache::SymbolIndexDb::global(cx);
            cx.background_spawn(async move { db.delete_files(uncached_paths).await.log_err() })
                .detach();
        }
        if removed_any {
            project_index.generation += 1;
            cx.notify();
        }
    }

    fn apply_results(
        &mut self,
        project_id: EntityId,
        results: Vec<IndexResult>,
        cx: &mut Context<Self>,
    ) {
        let Some(project_index) = self.projects.get_mut(&project_id) else {
            return;
        };
        for result in results {
            let key = (result.worktree_id, result.path);
            if result.file.symbols.is_empty() && result.file.todos.is_empty() {
                project_index.files.remove(&key);
            } else {
                let symbols = result
                    .file
                    .symbols
                    .into_iter()
                    .map(|symbol| IndexedSymbol {
                        name: intern(&mut self.interned_names, symbol.name),
                        kind: symbol.kind,
                    })
                    .collect();
                project_index.files.insert(
                    key,
                    IndexedFile {
                        symbols,
                        todos: result.file.todos.into(),
                    },
                );
            }
        }
        project_index.generation += 1;
        cx.notify();
    }

    pub fn indexed_symbol_count(&self, project: &Entity<Project>) -> usize {
        self.projects
            .get(&project.entity_id())
            .map_or(0, |project_index| {
                project_index
                    .files
                    .values()
                    .map(|file| file.symbols.len())
                    .sum()
            })
    }
}

struct IndexResult {
    worktree_id: WorktreeId,
    path: Arc<RelPath>,
    file: extract::FileIndex,
}

async fn run_indexer(
    this: gpui::WeakEntity<SymbolIndex>,
    project_id: EntityId,
    languages: Arc<LanguageRegistry>,
    fs: Arc<dyn Fs>,
    mut jobs: mpsc::UnboundedReceiver<IndexJob>,
    cx: &mut gpui::AsyncApp,
) {
    let db = cx.update(|cx| cache::SymbolIndexDb::global(cx));
    while let Some(first_job) = jobs.next().await {
        // Bursts of file events queue the same file repeatedly; index each one once per batch.
        let mut batch = HashMap::default();
        batch.insert((first_job.worktree_id, first_job.path.clone()), first_job);
        while batch.len() < INDEX_BATCH_SIZE
            && let Ok(job) = jobs.try_recv()
        {
            batch.insert((job.worktree_id, job.path.clone()), job);
        }

        let languages = languages.clone();
        let fs = fs.clone();
        let db = db.clone();
        let results = cx
            .background_spawn(async move {
                let mut results = Vec::with_capacity(batch.len());
                let mut new_rows = Vec::new();
                for job in batch.into_values() {
                    let cache_key = job.abs_path.to_string_lossy().into_owned();
                    let cached = job.mtime.and_then(|mtime| {
                        let (cached_mtime, cached_size, contents) =
                            db.cached_file(cache_key.clone()).log_err().flatten()?;
                        if cached_mtime != mtime || cached_size != job.size as i64 {
                            return None;
                        }
                        serde_json::from_slice::<extract::FileIndex>(&contents).log_err()
                    });
                    let file = match cached {
                        Some(file) => file,
                        None => {
                            let file = index_file(&job.abs_path, &languages, fs.as_ref()).await;
                            if let Some(mtime) = job.mtime
                                && let Some(contents) = serde_json::to_vec(&file).log_err()
                            {
                                new_rows.push(cache::CachedFileRow {
                                    path: cache_key,
                                    mtime,
                                    size: job.size as i64,
                                    contents,
                                });
                            }
                            file
                        }
                    };
                    results.push(IndexResult {
                        worktree_id: job.worktree_id,
                        path: job.path,
                        file,
                    });
                }
                db.save_files(new_rows).await.log_err();
                results
            })
            .await;

        if this
            .update(cx, |this, cx| this.apply_results(project_id, results, cx))
            .is_err()
        {
            return;
        }
    }
}

async fn index_file(
    abs_path: &std::path::Path,
    languages: &Arc<LanguageRegistry>,
    fs: &dyn Fs,
) -> extract::FileIndex {
    // Without a known language there's no grammar to find symbols or comments with.
    let Ok(language) = languages.load_language_for_file_path(abs_path).await else {
        return extract::FileIndex::default();
    };
    if language.grammar().is_none() {
        return extract::FileIndex::default();
    }
    let Some(text) = fs.load(abs_path).await.log_err() else {
        return extract::FileIndex::default();
    };
    extract::index_text(&language, &text)
}

fn intern(interned_names: &mut HashSet<Arc<str>>, name: Arc<str>) -> Arc<str> {
    if let Some(interned) = interned_names.get(&name) {
        return interned.clone();
    }
    interned_names.insert(name.clone());
    name
}

/// The index entity, for observing updates.
pub fn global_index(cx: &App) -> Option<Entity<SymbolIndex>> {
    cx.try_global::<GlobalSymbolIndex>()
        .map(|index| index.0.clone())
}

/// TODO comments in the project's indexed files, ordered by worktree and path.
pub fn project_todos(project: &Entity<Project>, cx: &App) -> Vec<FileTodos> {
    let Some(index) = cx.try_global::<GlobalSymbolIndex>() else {
        return Vec::new();
    };
    let Some(project_index) = index.0.read(cx).projects.get(&project.entity_id()) else {
        return Vec::new();
    };
    let mut todos = project_index
        .files
        .iter()
        .filter(|(_, file)| !file.todos.is_empty())
        .map(|((worktree_id, path), file)| FileTodos {
            worktree_id: *worktree_id,
            path: path.clone(),
            todos: file.todos.clone(),
        })
        .collect::<Vec<_>>();
    todos.sort_by(|left, right| {
        (left.worktree_id, &left.path).cmp(&(right.worktree_id, &right.path))
    });
    todos
}

/// Fuzzy-matches project symbols against `query`. The flag is true when results were capped at
/// `limit`, so callers can re-query as the user types instead of filtering a truncated list.
pub fn candidates(
    project: &Entity<Project>,
    query: &str,
    limit: usize,
    cx: &App,
) -> Task<(Vec<SymbolCandidate>, bool)> {
    let Some(index) = cx.try_global::<GlobalSymbolIndex>() else {
        return Task::ready((Vec::new(), false));
    };
    let Some(project_index) = index.0.read(cx).projects.get(&project.entity_id()) else {
        return Task::ready((Vec::new(), false));
    };
    if query.is_empty() || project_index.files.is_empty() {
        return Task::ready((Vec::new(), false));
    }

    let generation = project_index.generation;
    let name_table = project_index.name_table.clone();
    let cached_table = name_table
        .lock()
        .as_ref()
        .filter(|(table_generation, _)| *table_generation == generation)
        .map(|(_, table)| table.clone());
    let files = if cached_table.is_some() {
        Vec::new()
    } else {
        project_index
            .files
            .iter()
            .filter(|(_, file)| !file.symbols.is_empty())
            .map(|((_, path), file)| (path.clone(), file.symbols.clone()))
            .collect::<Vec<_>>()
    };
    let query = query.to_string();
    let executor = cx.background_executor().clone();
    cx.background_spawn(async move {
        let table = match cached_table {
            Some(table) => table,
            None => {
                let table = Arc::new(NameTable::build(&files));
                *name_table.lock() = Some((generation, table.clone()));
                table
            }
        };
        let smart_case = query.chars().any(|character| character.is_uppercase());
        let matches = fuzzy::match_strings(
            &table.match_candidates,
            &query,
            smart_case,
            true,
            limit,
            &AtomicBool::new(false),
            executor,
        )
        .await;
        let truncated = matches.len() >= limit;
        let symbols = matches
            .into_iter()
            .filter_map(|string_match| table.symbols.get(string_match.candidate_id).cloned())
            .collect();
        (symbols, truncated)
    })
}

#[cfg(test)]
mod symbol_index_tests;
