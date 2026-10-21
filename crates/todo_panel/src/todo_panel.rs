use anyhow::Result;
use editor::Editor;
use file_icons::FileIcons;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListSizingBehavior, Pixels, Render, ScrollStrategy, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, actions, px, uniform_list,
};
use language::Point;
use project::{Project, ProjectPath, WorktreeId};
use std::{ops::Range, sync::Arc};
use symbol_index::TodoItem;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt as _, paths::PathStyle, rel_path::RelPath};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    todo_panel,
    [
        /// Toggles focus on the TODO panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TodoPanel>(window, cx);
        });
    })
    .detach();
}

enum Row {
    File {
        path: Arc<RelPath>,
        todo_count: usize,
    },
    Todo {
        worktree_id: WorktreeId,
        path: Arc<RelPath>,
        todo: TodoItem,
    },
}

/// Lists the TODO and FIXME comments that the symbol index found in the project, like the
/// TODO tool window in JetBrains IDEs.
pub struct TodoPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    rows: Vec<Row>,
    todo_count: usize,
    selected_row: Option<usize>,
    scroll_handle: UniformListScrollHandle,
    position: DockPosition,
    _index_subscription: Option<Subscription>,
}

impl TodoPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            workspace.update(cx, |workspace, cx| cx.new(|cx| Self::new(workspace, cx)))
        })
    }

    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let index_subscription = symbol_index::global_index(cx)
            .map(|index| cx.observe(&index, |this, _, cx| this.refresh(cx)));
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project: workspace.project().clone(),
            focus_handle: cx.focus_handle(),
            rows: Vec::new(),
            todo_count: 0,
            selected_row: None,
            scroll_handle: UniformListScrollHandle::new(),
            position: DockPosition::Bottom,
            _index_subscription: index_subscription,
        };
        this.refresh(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let files = symbol_index::project_todos(&self.project, cx);
        self.rows.clear();
        self.todo_count = 0;
        for file in files {
            self.todo_count += file.todos.len();
            self.rows.push(Row::File {
                path: file.path.clone(),
                todo_count: file.todos.len(),
            });
            self.rows.extend(file.todos.iter().map(|todo| Row::Todo {
                worktree_id: file.worktree_id,
                path: file.path.clone(),
                todo: todo.clone(),
            }));
        }
        self.selected_row = self.selected_row.filter(|row| *row < self.rows.len());
        cx.notify();
    }

    fn select_next(&mut self, _: &menu::SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        let next_row = self
            .selected_row
            .map_or(0, |row| row + 1)
            .min(self.rows.len().saturating_sub(1));
        self.select_row(next_row, cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_row = self.selected_row.map_or(0, |row| row.saturating_sub(1));
        self.select_row(previous_row, cx);
    }

    fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if row >= self.rows.len() {
            return;
        }
        self.selected_row = Some(row);
        self.scroll_handle
            .scroll_to_item(row, ScrollStrategy::Center);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row {
            self.open_row(row, window, cx);
        }
    }

    fn open_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Row::Todo {
            worktree_id,
            path,
            todo,
        }) = self.rows.get(row)
        else {
            return;
        };
        let project_path = ProjectPath {
            worktree_id: *worktree_id,
            path: path.clone(),
        };
        let point = Point::new(todo.row, todo.column);
        let Some(open_task) = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace.open_path(project_path, None, true, window, cx)
            })
            .log_err()
        else {
            return;
        };
        cx.spawn_in(window, async move |_, cx| {
            let item = open_task.await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(point, window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|row_index| {
                let row = self.rows.get(row_index)?;
                let is_selected = self.selected_row == Some(row_index);
                let item = match row {
                    Row::File { path, todo_count } => {
                        let icon = FileIcons::get_icon(path.as_std_path(), cx)
                            .map(Icon::from_path)
                            .unwrap_or_else(|| Icon::new(IconName::File));
                        ListItem::new(row_index)
                            .spacing(ListItemSpacing::Sparse)
                            .start_slot(icon.color(Color::Muted).size(IconSize::Small))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(Label::new(path.display(PathStyle::local()).to_string()))
                                    .child(
                                        Label::new(todo_count.to_string())
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                    }
                    Row::Todo { todo, .. } => ListItem::new(row_index)
                        .spacing(ListItemSpacing::Sparse)
                        .indent_level(1)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(format!("{}", todo.row + 1))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(todo.text.to_string()).truncate()),
                        ),
                };
                Some(
                    item.toggle_state(is_selected)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.selected_row = Some(row_index);
                            this.open_row(row_index, window, cx);
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Render for TodoPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("todo-panel")
            .key_context("TodoPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(Label::new("TODO").size(LabelSize::Small))
                    .child(
                        Label::new(self.todo_count.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(if self.rows.is_empty() {
                h_flex()
                    .size_full()
                    .justify_center()
                    .child(Label::new("No TODO or FIXME comments found").color(Color::Muted))
                    .into_any_element()
            } else {
                uniform_list(
                    "todo-rows",
                    self.rows.len(),
                    cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)),
                )
                .with_sizing_behavior(ListSizingBehavior::Infer)
                .track_scroll(&self.scroll_handle)
                .size_full()
                .into_any_element()
            })
    }
}

impl Focusable for TodoPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TodoPanel {}

impl Panel for TodoPanel {
    fn persistent_name() -> &'static str {
        "TodoPanel"
    }

    fn panel_key() -> &'static str {
        "TodoPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("TODO Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}
