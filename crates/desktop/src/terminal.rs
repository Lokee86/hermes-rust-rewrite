//! The Iced terminal is a viewer. The authenticated profile gateway owns terminal access.
#[cfg(all(test, windows))]
pub(crate) mod acceptance;
mod wire;

use crate::appearance::text;
use iced::{
    Element, Fill, Size, Task,
    keyboard::{Key, key::Named},
    widget::{self, button, column, container, row, sensor, text_editor},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use wire::{Access, Command, Request};

const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;
const EDITOR_ID: &str = "hermes-terminal-surface";

#[derive(Clone)]
pub enum Action {
    New,
    AttachExisting(String),
    AddToChat(String),
    Select(u64),
    Close(u64),
    Input(Vec<u8>),
    Paste,
    Pasted(Option<String>),
    Display(text_editor::Action),
    Attached(u64, u64, String, String),
    Output(u64, u64, Arc<[u8]>),
    Disconnected(u64, u64, Option<String>),
    Closed(u64, u64, Result<(), String>),
    Resize(Size),
}
impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Terminal action")
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Scope {
    route: String,
    workspace: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Bookmark {
    scope: Scope,
    terminal_id: Option<String>,
    name: String,
}

impl Bookmark {
    fn save(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;
        let dir = path.parent().ok_or("Missing terminal bookmark directory")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut file, self).map_err(|e| e.to_string())?;
        file.flush()
            .and_then(|_| file.as_file().sync_all())
            .map_err(|e| e.to_string())?;
        file.persist(path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct Tab {
    name: String,
    bookmark: PathBuf,
    terminal_id: Option<String>,
    generation: u64,
    commands: Option<async_channel::Sender<Command>>,
    closing: bool,
    parser: vt100::Parser,
    editor: text_editor::Content,
    notice: Option<String>,
}
impl Tab {
    fn refresh(&mut self) {
        let (r, c) = self.parser.screen().cursor_position();
        let mut contents = self.parser.screen().contents();
        let n = contents.split('\n').count();
        if n <= r as usize {
            contents.extend(std::iter::repeat_n(
                '\n',
                (r as usize + 1).saturating_sub(n),
            ));
        }
        self.editor = text_editor::Content::with_text(&contents);
        self.editor.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: r.into(),
                column: c.into(),
            },
            selection: None,
        });
    }
}

#[derive(Clone)]
struct Target {
    root: PathBuf,
    scope: Scope,
    access: Access,
}

pub struct TerminalPane {
    target: Option<Target>,
    current: Option<u64>,
    tabs: BTreeMap<u64, Tab>,
    next: u64,
    error: Option<String>,
    rows: u16,
    cols: u16,
}

impl crate::Hermes {
    pub(super) fn sync_terminal_context(&mut self) {
        use crate::connections::Mode;
        let workspace = self.workspace();
        let target = self.library.as_ref().and_then(|library| {
            let root = library.root().to_owned();
            let state = &self.connections;
            let (route, access) = match state.config.mode {
                Mode::Local => (
                    "local".to_owned(),
                    Access::Local {
                        root: root.clone(),
                        provider: self.config.clone(),
                    },
                ),
                Mode::Remote | Mode::Ssh | Mode::Cloud => {
                    let (url, token) = match crate::remote::Route::selected(&root, state).ok()? {
                        crate::remote::Route::Local => return None,
                        crate::remote::Route::Remote { origin, token } => (origin, token),
                    };
                    let route = if state.config.mode == Mode::Ssh {
                        serde_json::to_string(&state.config.ssh).ok()?
                    } else {
                        url.clone()
                    };
                    (
                        route,
                        Access::Remote {
                            root: root.clone(),
                            url,
                            token,
                        },
                    )
                }
            };
            Some(Target {
                root,
                scope: Scope { route, workspace },
                access,
            })
        });
        self.terminal.bind(target);
    }
}

impl TerminalPane {
    pub fn new() -> Self {
        Self {
            target: None,
            current: None,
            tabs: BTreeMap::new(),
            next: 0,
            error: None,
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
        }
    }
    fn bind(&mut self, target: Option<Target>) {
        let same = self
            .target
            .as_ref()
            .zip(target.as_ref())
            .is_some_and(|(a, b)| a.root == b.root && a.scope == b.scope);
        self.target = target;
        if same {
            return;
        }
        self.tabs.clear();
        self.current = None;
        self.error = None;
        let Some(target) = &self.target else { return };
        let dir = target.root.join("terminal-tabs");
        let entries = match std::fs::read_dir(dir) {
            Ok(files) => files,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.error = Some(format!("Could not restore terminal tabs: {e}"));
                return;
            }
        };
        let mut files: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        for path in files {
            let Some(saved) = std::fs::read(&path)
                .ok()
                .and_then(|data| serde_json::from_slice::<Bookmark>(&data).ok())
            else {
                continue;
            };
            if saved.scope != target.scope
                || saved.terminal_id.as_deref() == Some("")
                || (saved.terminal_id.is_none() && !matches!(target.access, Access::Local { .. }))
            {
                continue;
            }
            self.next += 1;
            self.tabs.insert(
                self.next,
                Tab {
                    bookmark: path,
                    name: saved.name,
                    terminal_id: saved.terminal_id,
                    generation: 0,
                    commands: None,
                    closing: false,
                    parser: vt100::Parser::new(self.rows, self.cols, 1000),
                    editor: text_editor::Content::new(),
                    notice: Some("Select to reattach".into()),
                },
            );
            self.current = Some(self.next);
        }
    }
    fn current_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(&self.current?)
    }

    pub fn ensure_shell(&mut self) -> Task<Action> {
        if let Some(id) = self.current {
            self.update(Action::Select(id))
        } else {
            self.new_shell(None)
        }
    }
    pub fn account_shell(&mut self, command: String) -> Task<Action> {
        if !matches!(
            self.target.as_ref().map(|t| &t.access),
            Some(Access::Local { .. })
        ) {
            self.error =
                Some("Select the local gateway to sign in to a local provider CLI.".into());
            return Task::none();
        }
        self.new_shell(Some(format!("{command}\r").into_bytes()))
    }
    fn new_shell(&mut self, initial: Option<Vec<u8>>) -> Task<Action> {
        let Some(target) = &self.target else {
            self.error = Some("Connect to a profile gateway to open a terminal.".into());
            return Task::none();
        };
        if self.tabs.len() >= 16 {
            self.error = Some("Close a terminal before opening another.".into());
            return Task::none();
        }
        self.next += 1;
        let id = self.next;
        let bookmark = target
            .root
            .join("terminal-tabs")
            .join(format!("{}.json", uuid::Uuid::new_v4()));
        if matches!(target.access, Access::Local { .. }) {
            let pending = Bookmark {
                scope: target.scope.clone(),
                terminal_id: None,
                name: "Terminal".into(),
            };
            if let Err(e) = pending.save(&bookmark) {
                self.error = Some(format!("Could not save terminal launch: {e}"));
                return Task::none();
            }
        }
        self.tabs.insert(
            id,
            Tab {
                name: "Terminal".into(),
                bookmark,
                terminal_id: None,
                generation: 0,
                commands: None,
                closing: false,
                parser: vt100::Parser::new(self.rows, self.cols, 1000),
                editor: text_editor::Content::new(),
                notice: None,
            },
        );
        self.current = Some(id);
        self.attach(id, initial)
    }
    fn attach_existing(&mut self, terminal_id: String) -> Task<Action> {
        if !hermes_engine::herdr::valid_id(&terminal_id) {
            self.error = Some("Invalid agent terminal ID.".into());
            return Task::none();
        }
        let Some(target) = &self.target else {
            self.error = Some("Connect to a profile gateway to open an agent terminal.".into());
            return Task::none();
        };
        if let Some((&id, _)) = self.tabs.iter().find(|(_, tab)| tab.terminal_id.as_deref() == Some(&terminal_id)) {
            self.current = Some(id);
            return self.attach(id, None);
        }
        if self.tabs.len() >= 16 {
            self.error = Some("Close a terminal before opening another.".into());
            return Task::none();
        }
        self.next += 1;
        let id = self.next;
        let bookmark = target.root.join("terminal-tabs").join(format!("{}.json", uuid::Uuid::new_v4()));
        let saved = Bookmark { scope: target.scope.clone(), terminal_id: Some(terminal_id.clone()), name: "Agent PTY".into() };
        if let Err(error) = saved.save(&bookmark) {
            self.error = Some(format!("Could not save agent terminal bookmark: {error}"));
            return Task::none();
        }
        let tab = Tab {
            name: "Agent PTY".into(),
            bookmark,
            terminal_id: Some(terminal_id),
            generation: 0,
            commands: None,
            closing: false,
            parser: vt100::Parser::new(self.rows, self.cols, 1000),
            editor: text_editor::Content::new(),
            notice: Some("Connecting…".into()),
        };
        self.tabs.insert(id, tab);
        self.current = Some(id);
        self.attach(id, None)
    }
    fn request(&self, id: u64) -> Option<Request> {
        let target = self.target.as_ref()?;
        let tab = self.tabs.get(&id)?;
        Some(Request {
            access: target.access.clone(),
            scope: target.scope.clone(),
            bookmark: tab.bookmark.clone(),
            attach: tab.terminal_id.clone(),
            cols: self.cols,
            rows: self.rows,
        })
    }
    fn attach(&mut self, id: u64, initial: Option<Vec<u8>>) -> Task<Action> {
        let Some(request) = self.request(id) else {
            return Task::none();
        };
        let tab = self.tabs.get_mut(&id).unwrap();
        if tab.commands.is_some() || tab.closing {
            return widget::operation::focus(EDITOR_ID);
        }
        if tab.generation != 0
            && tab.terminal_id.is_none()
            && !matches!(request.access, Access::Local { .. })
        {
            // Stock remote owners may not support idempotent launch after a lost reply.
            return Task::none();
        }
        tab.generation += 1;
        let generation = tab.generation;
        let (commands, input) = async_channel::bounded(64);
        if let Some(bytes) = initial {
            let _ = commands.try_send(Command::Input(bytes));
        }
        tab.commands = Some(commands);
        tab.parser = vt100::Parser::new(self.rows, self.cols, 1000);
        tab.notice = Some("Connecting…".into());
        Task::batch([
            Task::run(
                iced::stream::channel(24, move |ui| request.stream(id, generation, input, ui)),
                |a| a,
            ),
            widget::operation::focus(EDITOR_ID),
        ])
    }
    fn forget(&mut self, id: u64) {
        if let Some(tab) = self.tabs.remove(&id) {
            if let Err(e) = std::fs::remove_file(&tab.bookmark)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                self.error = Some(format!("Could not remove the terminal bookmark: {e}"));
            }
        }
        if self.current == Some(id) {
            self.current = self.tabs.keys().next_back().copied();
        }
    }
    fn close(&mut self, id: u64) -> Task<Action> {
        let Some(request) = self.request(id) else {
            return Task::none();
        };
        let tab = self.tabs.get_mut(&id).unwrap();
        if tab.closing {
            return Task::none();
        }
        if let Some(commands) = &tab.commands {
            match commands.try_send(Command::End) {
                Ok(()) => tab.closing = true,
                Err(_) => self.error = Some("Terminal is busy. Try closing it again.".into()),
            }
        } else if tab.terminal_id.is_some() || matches!(request.access, Access::Local { .. }) {
            tab.closing = true;
            let generation = tab.generation;
            return Task::perform(async move { request.end().await }, move |r| {
                Action::Closed(id, generation, r)
            });
        } else {
            self.forget(id);
        }
        Task::none()
    }

    pub fn update(&mut self, action: Action) -> Task<Action> {
        match action {
            Action::New => return self.new_shell(None),
            Action::AttachExisting(terminal_id) => return self.attach_existing(terminal_id),
            Action::Select(id) if self.tabs.contains_key(&id) => {
                self.current = Some(id);
                return self.attach(id, None);
            }
            Action::Select(_) => {}
            Action::Close(id) => return self.close(id),
            Action::Attached(id, generation, terminal_id, shell) => {
                if let Some(tab) = self
                    .tabs
                    .get_mut(&id)
                    .filter(|t| t.generation == generation)
                {
                    if !shell.is_empty() {
                        tab.name = shell;
                    }
                    tab.terminal_id = Some(terminal_id);
                    tab.notice = None;
                }
            }
            Action::Output(id, generation, bytes) => {
                if let Some(tab) = self
                    .tabs
                    .get_mut(&id)
                    .filter(|t| t.generation == generation)
                {
                    tab.parser.process(&bytes);
                    tab.refresh();
                }
            }
            Action::Disconnected(id, generation, reason) => {
                if let Some(tab) = self
                    .tabs
                    .get_mut(&id)
                    .filter(|t| t.generation == generation)
                {
                    tab.commands = None;
                    tab.closing = false;
                    tab.notice = reason;
                }
            }
            Action::Closed(id, generation, result) => {
                if let Some(tab) = self
                    .tabs
                    .get_mut(&id)
                    .filter(|t| t.generation == generation)
                {
                    tab.closing = false;
                    match result {
                        Ok(()) => self.forget(id),
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            Action::AddToChat(_) => {}
            Action::Display(text_editor::Action::Scroll { lines }) => {
                if let Some(tab) = self.current_mut() {
                    let offset = tab
                        .parser
                        .screen()
                        .scrollback()
                        .saturating_add_signed(-(lines as isize));
                    tab.parser.screen_mut().set_scrollback(offset);
                    tab.refresh();
                }
            }
            Action::Display(action) => {
                if let Some(tab) = self.current_mut() {
                    if !matches!(action, text_editor::Action::Edit(_)) {
                        tab.editor.perform(action);
                    }
                }
            }
            Action::Input(bytes) => {
                if bytes.len() > 1_048_576 {
                    self.error = Some("Terminal input exceeds 1 MiB".into());
                } else if let Some(tab) = self.current_mut() {
                    if tab.parser.screen().scrollback() > 0 {
                        tab.parser.screen_mut().set_scrollback(0);
                        tab.refresh();
                    }
                    match &tab.commands {
                        Some(commands) if !tab.closing => {
                            if commands.try_send(Command::Input(bytes)).is_err() {
                                self.error = Some("Terminal input busy or disconnected".into());
                            }
                        }
                        _ => {
                            self.error =
                                Some("Select the terminal tab to reattach before typing.".into())
                        }
                    }
                }
            }
            Action::Paste => return iced::clipboard::read().map(Action::Pasted),
            Action::Pasted(Some(data)) => {
                let bytes = if self
                    .current
                    .and_then(|id| self.tabs.get(&id))
                    .is_some_and(|t| t.parser.screen().bracketed_paste())
                {
                    format!("\x1b[200~{data}\x1b[201~").into_bytes()
                } else {
                    data.into_bytes()
                };
                return self.update(Action::Input(bytes));
            }
            Action::Pasted(None) => {}
            Action::Resize(size) => {
                let cols = ((size.width - 12.0) / 7.8).floor().clamp(12.0, 300.0) as u16;
                let rows = ((size.height - 10.0) / 17.0).floor().clamp(3.0, 150.0) as u16;
                self.cols = cols;
                self.rows = rows;
                if let Some(tab) = self.current_mut() {
                    if tab.parser.screen().size() != (rows, cols) {
                        if let Some(commands) = &tab.commands {
                            if commands.try_send(Command::Resize { rows, cols }).is_err() {
                                tab.notice = Some("Terminal resize could not be delivered".into());
                            }
                        }
                        tab.parser.screen_mut().set_size(rows, cols);
                        tab.refresh();
                    }
                }
            }
        }
        Task::none()
    }

    pub fn view(
        &self,
        shortcuts: &crate::shortcuts::Bindings,
        capturing: bool,
    ) -> Element<'_, Action> {
        let shortcuts = shortcuts.clone();
        let mut rail = row![].spacing(2);
        for (&id, t) in self.tabs.iter() {
            rail = rail
                .push(
                    button(text(&t.name).size(11))
                        .on_press(Action::Select(id))
                        .style(button::text),
                )
                .push(
                    button(text("×").size(12))
                        .on_press(Action::Close(id))
                        .style(button::text),
                );
        }
        rail = rail.push(
            button(text("+").size(15))
                .on_press(Action::New)
                .style(button::text),
        );
        let mut body = column![rail].spacing(3);
        if let Some(error) = &self.error {
            body = body.push(text(error).size(12));
        }
        if let Some(tab) = self.current.and_then(|id| self.tabs.get(&id)) {
            if let Some(selection) = tab.editor.selection()
                && !selection.trim().is_empty()
            {
                body = body.push(
                    button(text("Add to chat").size(11))
                        .on_press(Action::AddToChat(selection))
                        .style(button::text),
                );
            }
            let editor = text_editor(&tab.editor)
                .id(EDITOR_ID)
                .on_action(Action::Display)
                .key_binding(move |press| {
                    if capturing
                        || shortcuts
                            .action_for_key(&press.key, press.physical_key, press.modifiers, true)
                            .is_some()
                    {
                        None
                    } else {
                        terminal_key(press)
                    }
                })
                .font(crate::appearance::terminal_font())
                .size(13)
                .line_height(iced::widget::text::LineHeight::Absolute(17.0.into()))
                .padding(5)
                .height(Fill)
                .style(|theme, status| {
                    let mut s = text_editor::default(theme, status);
                    s.border = iced::Border::default();
                    s
                });
            body = body.push(
                sensor(editor)
                    .on_resize(Action::Resize)
                    .on_show(Action::Resize),
            );
            if let Some(end) = &tab.notice {
                body = body.push(text(end).size(11));
            }
        } else {
            body = body.push(container(button("New terminal").on_press(Action::New)).center(Fill));
        }
        container(body).padding(4).width(Fill).height(Fill).into()
    }
}

fn terminal_key(key: text_editor::KeyPress) -> Option<text_editor::Binding<Action>> {
    use text_editor::Binding;
    if key.modifiers.logo() || (key.modifiers.control() && key.modifiers.shift()) {
        return match key.key.as_ref() {
            Key::Character("c") => Some(Binding::Copy),
            Key::Character("v") => Some(Binding::Custom(Action::Paste)),
            Key::Character("a") => Some(Binding::SelectAll),
            _ => None,
        };
    }
    if key.modifiers.control() {
        if let Key::Character(ch) = key.key.as_ref() {
            if ch.len() == 1 && ch.as_bytes()[0].is_ascii_alphabetic() {
                return Some(Binding::Custom(Action::Input(vec![
                    ch.as_bytes()[0].to_ascii_uppercase() & 31,
                ])));
            }
        }
    }
    let bytes = match key.key.as_ref() {
        Key::Named(Named::Enter) => b"\r".to_vec(),
        Key::Named(Named::Backspace) => vec![127],
        Key::Named(Named::Tab) => b"\t".to_vec(),
        Key::Named(Named::Escape) => vec![27],
        Key::Named(Named::ArrowUp) => b"\x1b[A".to_vec(),
        Key::Named(Named::ArrowDown) => b"\x1b[B".to_vec(),
        Key::Named(Named::ArrowRight) => b"\x1b[C".to_vec(),
        Key::Named(Named::ArrowLeft) => b"\x1b[D".to_vec(),
        Key::Named(Named::Home) => b"\x1b[H".to_vec(),
        Key::Named(Named::End) => b"\x1b[F".to_vec(),
        Key::Named(Named::Delete) => b"\x1b[3~".to_vec(),
        _ => key.text?.as_bytes().to_vec(),
    };
    Some(Binding::Custom(Action::Input(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn emulator_applies_terminal_control_sequences() {
        let mut p = vt100::Parser::new(4, 20, 0);
        p.process(b"old\r\x1b[2Knew");
        assert_eq!(p.screen().contents(), "new");
    }

    #[test]
    fn agent_pty_action_attaches_existing_identity_without_launching_shell() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_path_buf();
        let mut pane = TerminalPane::new();
        pane.target = Some(Target {
            root: path.clone(),
            scope: Scope { route: "local".into(), workspace: Some(path.clone()) },
            access: Access::Local { root: path, provider: hermes_engine::Config::from_env() },
        });
        let terminal_id = "A".repeat(43);
        let _task = pane.update(Action::AttachExisting(terminal_id.clone()));
        let tab = &pane.tabs[&pane.current.unwrap()];
        assert_eq!(tab.terminal_id.as_deref(), Some(terminal_id.as_str()));
        assert_eq!(tab.generation, 1);
        assert_eq!(tab.name, "Agent PTY");
        assert_eq!(pane.tabs.len(), 1);
        let saved: Bookmark = serde_json::from_slice(&std::fs::read(&tab.bookmark).unwrap()).unwrap();
        assert_eq!(saved.terminal_id.as_deref(), Some(terminal_id.as_str()));
    }
}
