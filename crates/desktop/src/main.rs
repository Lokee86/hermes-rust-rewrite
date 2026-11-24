#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
//! Native Hermes client. Execution and durable transcripts belong to the engine.
mod ui;
mod sync;
mod hud_native;
mod wake_model;
mod files;
mod imports;
#[cfg(target_os = "macos")]
mod picker;
#[cfg(not(target_os = "macos"))]
#[path="picker_portable.rs"] mod picker;
mod git;
mod welcome;
mod settings;
mod models;
mod reasoning;
mod voice;
mod terminal;
mod artifacts;
mod queue;
mod statusbar;
mod appearance;
mod tabs;
mod attachments;
mod commands;
mod shortcuts;
mod vault;
mod backdrop;
mod command_center;
mod profile_settings;
mod notifications;
mod billing;
mod capabilities;
#[cfg(target_os="macos")] mod window_bounds;
mod scheduled;
mod local_gateway;
mod checkpoint_ui;
mod messaging;
mod backup;
mod project_discovery;
mod date_groups;
mod bots;
mod shell_art;
mod connections;
mod onboarding;
mod slash;
mod platforms;
mod webview;
mod remote;
mod chat_view;
mod ssh_tunnel;
#[cfg(test)]
mod interaction_tests;
#[cfg(test)]
mod ui_performance;
#[cfg(test)]
mod screens;
mod hud;
mod accounts;
mod webhooks;
mod profiles;
mod starmap;
mod quick_entry;
mod overlays;
mod routines;
mod pets;
mod routine_chats;
mod gateway_oauth;

use hermes_engine::{CancellationToken, Config, Entry, Event, History, Session, ToolCall, portal};
use hermes_library::{ArchiveFilter, Library, Project, SessionQuery, SessionSource, SessionSummary};
use iced::{Element, Subscription, Task, Theme, keyboard, widget::{self, markdown, text_editor}};
use serde::{Deserialize, Serialize};
use std::{fmt, path::{Path, PathBuf}, sync::Arc};

const SCROLL_ID: &str = "conversation";
const COMPOSER_ID: &str = "composer";
/// Sidebar › Cron jobs section state.
#[derive(Default)]
struct CronSidebar { open: bool, visible: usize, hover: Option<String>, peek: Option<String>, menu: Option<String> }
#[derive(Debug, Clone)]
enum CronUi { Toggle, More, Hover(Option<String>), Peek(String), Menu(String), Trigger(String), Pause(String, bool), Manage(String), Delete(String) }
/// Quick Entry dismisses on focus loss or Escape (only its own window acts on these).
fn quick_entry_window_event(event: iced::Event, _status: iced::event::Status, id: iced::window::Id) -> Option<Message> {
    match event {
        iced::Event::Window(iced::window::Event::Unfocused) => Some(Message::QuickEntry(quick_entry::Action::Unfocused(id))),
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), .. }) => Some(Message::QuickEntry(quick_entry::Action::Unfocused(id))),
        _ => None,
    }
}
fn secondary_session() -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() { if arg == "--open-session" { return args.next().filter(|id| !id.is_empty() && id.len() < 200 && !id.starts_with('-')); } }
    None
}
/// Separates a group-chat turn's visible text, the replying bot and its private instruction.
const GROUP_MARK: char = '\u{2063}';
/// (what the user typed, the bot replying) for a group-chat turn; plain turns pass through.
fn group_prompt(prompt: &str) -> (&str, Option<&str>) {
    let mut parts = prompt.splitn(3, GROUP_MARK);
    let visible = parts.next().unwrap_or_default();
    match (parts.next(), parts.next()) { (Some(name), Some(_)) => (visible, Some(name)), _ => (prompt, None) }
}

fn main() -> iced::Result {
    if let Some(code) = hermes_gateway::owner::dispatch_role() { std::process::exit(code); }
    // Owner processes must reach their entry point before Iced creates a window.
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "gateway") {
        let result = tokio::runtime::Runtime::new().map_err(anyhow::Error::from)
            .and_then(|runtime| runtime.block_on(hermes_gateway::owner::serve(std::env::args().skip(2))));
        if let Err(error) = result { eprintln!("Gateway startup failed: {error:#}"); std::process::exit(1); }
        return Ok(());
    }
    // A daemon: the app window plus the HUD band (its own window, as in Electron).
    iced::daemon(Hermes::new, Hermes::update_synced, Hermes::view_window)
        .title(|app: &Hermes, id: iced::window::Id| if app.hud.window == Some(id) { "Hermes HUD".to_owned() } else { "Hermes".to_owned() })
        .subscription(Hermes::subscription).theme(|app: &Hermes, _: iced::window::Id| ui::theme_for(app.dark))
        // Windows paint their own backgrounds, so the pet and wake pill can be see-through.
        .style(|app: &Hermes, _theme: &iced::Theme| iced::theme::Style { background_color: iced::Color::TRANSPARENT, text_color: ui::palette(app.dark).foreground })
        .font(include_bytes!("../assets/fonts/codicon.ttf").as_slice()).font(welcome::FONT_BYTES)
        .font(appearance::MONO_FONT_BYTES).font(appearance::UI_FONT_FILES[0]).font(appearance::UI_FONT_FILES[1]).font(appearance::UI_FONT_FILES[2]).font(appearance::UI_FONT_FILES[3]).default_font(appearance::UI_FONT).scale_factor(|app: &Hermes, _: iced::window::Id| app.appearance.preferences.ui_scale as f32 / 100.0)
        .run()
}

fn window_settings() -> iced::window::Settings {
    #[cfg(target_os="macos")]
    let (size,position)=window_bounds::initial(Session::default_dir().ok().as_deref());
    #[cfg(not(target_os="macos"))]
    let (size,position)=(iced::Size::new(1280.0,820.0),iced::window::Position::Centered);
    let settings = iced::window::Settings {
        size, min_size: Some(iced::Size::new(400.0, 620.0)),
        exit_on_close_request: false, position, ..Default::default()
    };
    #[cfg(feature = "ui-test")]
    let settings = if std::env::var_os("HERMES_RUST_TEST_CAPTURE").is_some() {
        iced::window::Settings { position: iced::window::Position::Specific(iced::Point::new(-9000.0, -9000.0)), ..settings }
    } else { settings };
    #[cfg(target_os = "macos")]
    let settings = iced::window::Settings { platform_specific: iced::window::settings::PlatformSpecific {
        title_hidden: true, titlebar_transparent: true, fullsize_content_view: true,
    }, ..settings };
    settings
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen { Chat, Provider, Workspaces, Tools, Files, Git, Terminal, Import, Messaging, Artifacts, Cron, Bots, About, CommandCenter, Agents, Webhooks, Profiles, Starmap, Pets }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RightPane { Files, Review, Terminal }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Preferences { base_url: String, model: String }

impl Preferences {
    fn path() -> Result<PathBuf, String> {
        Session::default_dir().map(|p| p.join("desktop-preferences.json")).map_err(|e| e.to_string())
    }
    fn load() -> Config {
        let mut config = Config::from_env();
        let explicit = std::env::var_os("HERMES_RUST_BASE_URL").is_some()
            || std::env::var_os("HERMES_RUST_API_KEY").is_some()
            || std::env::var_os("OPENROUTER_API_KEY").is_some();
        if !explicit {
            config.base_url = portal::INFERENCE.into();
            config.model = std::env::var("HERMES_RUST_MODEL").unwrap_or_else(|_| portal::AUTO_MODEL.into());
            config.api_key.clear();
        }
        if let Ok(path) = Self::path()
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(saved) = serde_json::from_slice::<Self>(&bytes) {
            if std::env::var_os("HERMES_RUST_BASE_URL").is_none() { config.base_url = saved.base_url; }
            if std::env::var_os("HERMES_RUST_MODEL").is_none() { config.model = saved.model; }
            let env = Config::from_env();
            if config.base_url.trim_end_matches('/') != env.base_url.trim_end_matches('/') { config.api_key.clear(); }
        }
        config
    }
    fn save(config: &Config) -> Result<(), String> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).map_err(|e| e.to_string())?; }
        // There is deliberately no key field. The secret never enters preferences.
        let saved = Self { base_url: config.base_url.clone(), model: config.model.clone() };
        let bytes = serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?;
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(temp, path).map_err(|e| e.to_string())
    }
}

#[derive(Clone)]
enum Message {
    Hud(hud::Action), SyncTick, #[cfg(feature = "ui-test")] HudTestInput, WindowClose(iced::window::Id), Account(accounts::Action), OpenInWindow(String), SubagentOpen(Option<String>), QuickEntry(quick_entry::Action), Overlay(overlays::Action), Pet(pets::Action), OpenRoutineChat(String), Cron(CronUi), Webhook(webhooks::Action), Profile(profiles::Action), Starmap(starmap::Action), WindowId(Option<iced::window::Id>), WebviewBounds(Option<iced::Rectangle>), WebviewTick, Slash(slash::Action), Onboarding(onboarding::Action), PortalAccent(shell_art::Accent), PortalArtwork(shell_art::Artwork), Connection(connections::Action), Bot(bots::Action), Reasoning(reasoning::Action), Notification(notifications::Action), Billing(billing::Action), Capability(capabilities::Action),
    Edit(text_editor::Action), Send, Stop,
    Engine(u64, Event), Finished(u64, History, Result<(), String>), Approve(String, u64, bool), Gateway(u64, remote::Update),
    NewChat, SelectChat(String), Search(String), Navigate(Screen), GatewayQueue(String, String, chat_view::QueueAction),
    ChoosePortal(bool), BaseUrl(String), Model(String), ApiKey(String), SaveProvider,
    Models(models::Action),
    Voice(voice::Action),
    PortalStart, PortalBegun(u64, Result<portal::DeviceLogin, String>), PortalFinished(u64, Result<(), String>), PortalCancel,
    WorkspacePath(String), AddWorkspace, SelectWorkspace(Option<String>), BrowseWorkspace, ProjectDiscovery(project_discovery::Action),
    RenameWorkspace(String), WorkspaceName(String), SaveWorkspaceName, CancelWorkspaceRename, WorkspaceMemorySynced(Result<serde_json::Value,String>),
    ArchiveResults(ArchiveScope, Result<serde_json::Value, String>), DismissArchiveResults,
    FolderChosen(Result<Option<PathBuf>, String>), Dismiss,
    RenameStart, RenameChanged(String), RenameSave, Copy(String), Link(String),
    Scrolled(bool), JumpToLatest, Shortcut(keyboard::Event),
    Files(files::Action), Git(git::GitMessage), Import(imports::Message), Close,
    SystemTheme(iced::theme::Mode), ToggleSidebar, FlipPanes, ToggleLayout, ToggleAttach, DragWindow,
    SettingsView(settings::View),
    SettingsSearch(String), ProfileSetting(profile_settings::Action), Checkpoint(checkpoint_ui::Action),
    SettingsApply,
    Terminal(terminal::Action), OpenAgentTerminal(String), Artifacts(artifacts::Message), Scheduled(scheduled::Action), Messaging(messaging::Message), LocalGateway(local_gateway::Action), Backup(backup::Action),
    Attachment(attachments::Action),
    ToggleBackdrop, BackdropLoaded(Result<iced::widget::image::Handle,String>),
    DraftIdle(u64), IntroSize(iced::Size),
    QueueCurrent, QueueRemove(u64), QueueSendNow(u64), QueueResume, QueuePause,
    QueueEdit(u64), QueueEditChanged(String), QueueEditSave, QueueEditCancel,
    SessionMenu(String), TogglePin(String), ArchiveSession(String, bool),
    CatalogPinned(String, bool, Result<serde_json::Value, String>),
    CatalogSessionChanged(String, bool, Result<serde_json::Value, String>),
    RenameFor(String),
    CommandSearch(String), CommandSource(command_center::Source), CommandRefresh, CommandOpenSession(String),
    ToggleSessionFilter, ArchiveFilter(ArchiveFilter),
    Statusbar(statusbar::Action),
    Appearance(appearance::Action),
    Tab(tabs::Action),
    OpenCommands, Commands(commands::Message), FocusCommands,
    Shortcuts(shortcuts::Message), GlobalKey(keyboard::Event, iced::event::Status),
    Vault(vault::Message),
    #[cfg(feature = "ui-test")]
    Capture,
    #[cfg(feature = "ui-test")]
    Captured(iced::window::Screenshot),
}

// API-key messages must not be exposed by widget diagnostics or debug logs.
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApiKey(_) => "ApiKey([redacted])", Self::SyncTick => "SyncTick", #[cfg(feature = "ui-test")] Self::HudTestInput => "HudTestInput", Self::Edit(_) => "Edit", Self::Send => "Send", Self::Stop => "Stop",
            Self::Engine(..) => "Engine", Self::Finished(..) => "Finished", Self::Approve(..) => "Approve", Self::Gateway(..) => "Gateway",
            Self::NewChat => "NewChat", Self::SelectChat(_) => "SelectChat", Self::Search(_) => "Search", Self::GatewayQueue(..) => "GatewayQueue",
            Self::Navigate(_) => "Navigate", Self::ChoosePortal(_) => "ChoosePortal", Self::BaseUrl(_) => "BaseUrl",
            Self::Model(_) => "Model", Self::SaveProvider => "SaveProvider", Self::PortalStart => "PortalStart",
            Self::Models(_) => "ProviderAction",
            Self::Voice(_) => "VoiceAction",
            Self::PortalBegun(..) => "PortalBegun", Self::PortalFinished(..) => "PortalFinished", Self::PortalCancel => "PortalCancel",
            Self::WorkspacePath(_) => "WorkspacePath", Self::AddWorkspace => "AddWorkspace", Self::SelectWorkspace(_) => "SelectWorkspace",
            Self::RenameWorkspace(_) => "RenameWorkspace", Self::WorkspaceName(_) => "WorkspaceName", Self::SaveWorkspaceName => "SaveWorkspaceName",
            Self::CancelWorkspaceRename => "CancelWorkspaceRename", Self::WorkspaceMemorySynced(_) => "WorkspaceMemorySynced",
            Self::ArchiveResults(..) => "ArchiveResults", Self::DismissArchiveResults => "DismissArchiveResults",
            Self::BrowseWorkspace => "BrowseWorkspace", Self::FolderChosen(_) => "FolderChosen", Self::ProjectDiscovery(_) => "ProjectDiscovery",
            Self::Dismiss => "Dismiss", Self::RenameStart => "RenameStart", Self::RenameChanged(_) => "RenameChanged",
            Self::RenameSave => "RenameSave", Self::Copy(_) => "Copy", Self::Link(_) => "Link",
            Self::Scrolled(_) => "Scrolled", Self::JumpToLatest => "JumpToLatest", Self::Shortcut(_) => "Shortcut",
            Self::Files(_) => "Files", Self::Git(_) => "Git", Self::Import(_) => "Import", Self::Close => "Close",
            Self::SystemTheme(_) => "SystemTheme", Self::ToggleSidebar => "ToggleSidebar", Self::FlipPanes => "FlipPanes",
            Self::Hud(_)=>"Hud", Self::WindowClose(_)=>"WindowClose", Self::Account(_)=>"Account", Self::OpenInWindow(_)=>"OpenInWindow", Self::SubagentOpen(_)=>"SubagentOpen", Self::QuickEntry(_)=>"QuickEntry", Self::Overlay(_)=>"Overlay", Self::Pet(_)=>"Pet", Self::OpenRoutineChat(_)=>"OpenRoutineChat", Self::Cron(_)=>"Cron", Self::Webhook(_)=>"Webhook", Self::Profile(_)=>"Profile", Self::Starmap(_)=>"Starmap", Self::WindowId(_)=>"WindowId", Self::WebviewBounds(_)=>"WebviewBounds", Self::WebviewTick=>"WebviewTick", Self::Slash(_)=>"Slash", Self::Onboarding(_)=>"Onboarding", Self::PortalAccent(_)=>"PortalAccent", Self::PortalArtwork(_)=>"PortalArtwork", Self::Connection(_)=>"Connection", Self::Bot(_)=>"Bot", Self::Reasoning(_)=>"Reasoning", Self::Notification(_)=> "Notification", Self::Billing(_)=>"Billing", Self::Capability(_)=>"Capability", Self::ToggleLayout => "ToggleLayout", Self::ToggleAttach => "ToggleAttach", Self::DragWindow => "DragWindow", Self::SettingsView(_) => "SettingsView", Self::SettingsSearch(_) => "SettingsSearch", Self::ProfileSetting(_) => "ProfileSetting", Self::SettingsApply => "SettingsApply",
            Self::Terminal(_) => "Terminal", Self::OpenAgentTerminal(_) => "OpenAgentTerminal", Self::Artifacts(_) => "Artifacts",
            Self::ToggleBackdrop => "ToggleBackdrop", Self::BackdropLoaded(_) => "BackdropLoaded", Self::DraftIdle(_) => "DraftIdle", Self::IntroSize(_) => "IntroSize",
            Self::Attachment(_) => "Attachment", Self::Scheduled(_) => "Scheduled", Self::Messaging(_) => "Messaging", Self::LocalGateway(_) => "LocalGateway", Self::Backup(_) => "Backup",
            Self::QueueCurrent => "QueueCurrent", Self::QueueRemove(_) => "QueueRemove", Self::QueueSendNow(_) => "QueueSendNow", Self::QueueResume => "QueueResume", Self::QueuePause => "QueuePause",
            Self::QueueEdit(_) => "QueueEdit", Self::QueueEditChanged(_) => "QueueEditChanged", Self::QueueEditSave => "QueueEditSave", Self::QueueEditCancel => "QueueEditCancel",
            Self::SessionMenu(_) => "SessionMenu", Self::TogglePin(_) => "TogglePin", Self::ArchiveSession(..) => "ArchiveSession",
            Self::CatalogPinned(..) => "CatalogPinned", Self::CatalogSessionChanged(..) => "CatalogSessionChanged", Self::RenameFor(_) => "RenameFor", Self::ToggleSessionFilter => "ToggleSessionFilter", Self::ArchiveFilter(_) => "ArchiveFilter",
            Self::CommandSearch(_) => "CommandSearch", Self::CommandSource(_) => "CommandSource", Self::CommandRefresh => "CommandRefresh", Self::CommandOpenSession(_) => "CommandOpenSession",
            Self::Statusbar(_) => "Statusbar",
            Self::Appearance(_) => "Appearance",
            Self::Tab(_) => "SessionTab",
            Self::OpenCommands => "OpenCommands", Self::Commands(_) => "Commands", Self::FocusCommands => "FocusCommands",
            Self::Shortcuts(_) => "Shortcuts", Self::GlobalKey(..) => "GlobalKey",
            Self::Vault(_) => "Vault([redacted])", Self::Checkpoint(_) => "Checkpoint",
            #[cfg(feature = "ui-test")]
            Self::Capture => "Capture",
            #[cfg(feature = "ui-test")]
            Self::Captured(_) => "Captured",
        })
    }
}

struct Approval { call: ToolCall, generation: u64, answer: Option<async_channel::Sender<bool>> }
struct Banner { text: String, error: bool }

#[derive(Clone, PartialEq, Eq)]
struct ArchiveScope {
    profile_root: PathBuf,
    session_id: Option<String>,
    project_id: Option<String>,
}

struct Hermes {
    intro_width:f32, backdrop: bool, backdrop_image: Option<iced::widget::image::Handle>, portal_connected: bool, draft_revision:u64, draft_save_pending:bool,
    library: Option<Library>, sessions: Vec<SessionSummary>, current: Option<SessionSummary>,
    project: Option<Project>, projects: Vec<Project>,
    session: Option<Arc<tokio::sync::Mutex<Session>>>, history: History,
    rendered: Vec<Vec<Option<markdown::Content>>>, composer: text_editor::Content,
    config: Config, screen: Screen, search: String, workspace_path: String,
    workspace_rename: Option<(String, String)>,
    archive_results: Option<(ArchiveScope, Vec<(String, String)>)>,
    banner: Option<Banner>, approvals: Vec<Approval>, chat: chat_view::View, cancellation: Option<CancellationToken>,
    run_id: u64, running: bool, stopping: bool, follow_stream: bool, rename: Option<String>, browsing: bool,
    portal_login: Option<portal::DeviceLogin>, portal_cancel: Option<CancellationToken>, portal_starting: bool, portal_id: u64,
    files: files::Files, git: git::GitPanel, imports: imports::ImportPanel, terminal: terminal::TerminalPane, artifacts: artifacts::Artifacts, closing: bool,
    dark: bool, sidebar_open: bool, flipped: bool, right_pane: Option<RightPane>, layout_menu: bool, attach_menu: bool,
    capabilities:capabilities::State, billing:billing::State, notifications:notifications::State, profile_settings:profile_settings::State, settings_view: settings::View,
    settings_query: String, fresh_draft: bool,
    models: models::State,
    voice: voice::State,
    queue: queue::PromptQueue, draft_files: attachments::DraftFiles, queue_edit: Option<(u64, String)>, redirecting: bool,
    session_menu: Option<String>, session_filter_menu: bool, archive_filter: ArchiveFilter, modifiers: keyboard::Modifiers,
    command_search: String, command_source: command_center::Source, command_sessions: Vec<SessionSummary>, command_error: Option<String>,
    reasoning:reasoning::State, return_to_composer:bool,
    /// Group chat: the members still to reply to the current message, in order.
    group_round: Option<std::collections::VecDeque<String>>,
    hud: hud::State,
    sync: sync::State,
    /// A window opened with `--open-session`: it keeps no tab list and runs no background services.
    secondary: bool,
    accounts: accounts::State,
    webhooks: webhooks::State,
    profiles: profiles::State,
    starmap: starmap::State,
    /// Live child agents of the current chat's turn (Agents page spawn tree).
    subagents: Vec<hermes_engine::subagents::Progress>,
    subagent_open: Option<String>,
    quick_entry: quick_entry::State,
    overlays: overlays::State,
    pets: pets::State,
    cron_ui: CronSidebar,
    statusbar: statusbar::State, bots: bots::State, artwork: Option<shell_art::Artwork>, connections: connections::State, onboarding: onboarding::State, slash: slash::State, webview: webview::Host,
    appearance: appearance::State, system_dark: bool,
    tabs: tabs::State,
    commands: commands::Commands, palette_open_folder: bool,
    shortcuts: shortcuts::State,
    vault: vault::State,
    scheduled: scheduled::Scheduled,
    local_gateway: local_gateway::State,
    checkpoints: checkpoint_ui::State,
    messaging: messaging::Messaging,
    backup: backup::State,
    project_discovery: project_discovery::State,
}

impl Hermes {
    fn new() -> (Self, Task<Message>) {
        let mut app = Self {
            intro_width: 960.0, backdrop: Session::default_dir().ok().is_some_and(|dir|backdrop::enabled(&dir)), backdrop_image:None, portal_connected:portal::is_available(), draft_revision:0, draft_save_pending:false,
            library: None, sessions: vec![], current: None, project: None, projects: vec![],
            session: None, history: History::default(), rendered: vec![], composer: text_editor::Content::new(),
            config: Preferences::load(), screen: Screen::Chat, search: String::new(), workspace_path: String::new(),
            workspace_rename: None,
            archive_results: None,
            banner: None, approvals: vec![], chat: chat_view::View::default(), cancellation: None, run_id: 0, running: false, stopping: false,
            follow_stream: true, rename: None, browsing: false, portal_login: None, portal_cancel: None, portal_starting: false, portal_id: 0,
            files: files::Files::new(), git: git::GitPanel::new(PathBuf::new()), imports: imports::ImportPanel::default(), terminal: terminal::TerminalPane::new(), artifacts: artifacts::Artifacts::default(), closing: false,
            dark: false, sidebar_open: true, flipped: false, right_pane: None, layout_menu: false, attach_menu: false,
            capabilities:capabilities::State::default(), billing:billing::State::default(), notifications:notifications::State::default(), profile_settings:profile_settings::State::default(), settings_view: settings::View::default(),
            settings_query: String::new(), fresh_draft: false,
            models: models::State::default(),
            voice: voice::State::open(),
            queue: queue::PromptQueue::default(), draft_files: attachments::DraftFiles::default(), queue_edit: None, redirecting: false,
            session_menu: None, session_filter_menu: false, archive_filter: ArchiveFilter::Active, modifiers: keyboard::Modifiers::default(),
            command_search: String::new(), command_source: command_center::Source::All, command_sessions: Vec::new(), command_error: None,
            reasoning:reasoning::State::default(), return_to_composer:false, group_round: None, hud: hud::State::default(), sync: sync::State::default(), secondary: false, accounts: accounts::State::default(), webhooks: webhooks::State::default(), profiles: profiles::State::default(), starmap: starmap::State::default(), subagents: Vec::new(), subagent_open: None, quick_entry: quick_entry::State::default(), overlays: overlays::State::default(), pets: pets::State::default(), cron_ui: CronSidebar { open: true, visible: 3, ..Default::default() }, statusbar: statusbar::State::default(), bots: bots::State::default(), artwork: None, connections: connections::State::default(), onboarding: onboarding::State::default(), slash: slash::State::default(), webview: webview::Host::default(),
            appearance: appearance::State::default(), system_dark: false,
            tabs: tabs::State::default(),
            commands: commands::Commands::default(), palette_open_folder: false,
            shortcuts: shortcuts::State::default(),
            vault: vault::State::default(),
            scheduled: scheduled::Scheduled::new(),
            local_gateway: local_gateway::State::default(),
            checkpoints: checkpoint_ui::State::default(),
            messaging: messaging::Messaging::default(),
            backup: backup::State::default(),
            project_discovery: project_discovery::State::default(),
        };
        match Library::open_default() {
            Ok(library) => {
                app.statusbar = statusbar::State::load(library.root());
                app.bots.refresh(Some(&library));
                let choices = shell_art::load_choices(library.root());
                app.artwork = Some(shell_art::chosen_artwork(&choices).unwrap_or_else(|| shell_art::for_launch(library.root())));
                app.connections = connections::State::load(library.root());
                app.onboarding = onboarding::State::load(library.root());
                app.appearance = appearance::State::load(library.root());
                app.dark = app.appearance.preferences.dark(app.system_dark);
                app.shortcuts = shortcuts::State::load(library.root());
                app.profile_settings=profile_settings::State::load(library.root());
                app.project_discovery=project_discovery::State::load(library.root());
                app.notifications=notifications::State::load(library.root());
                app.project = library.selected_project().unwrap_or_default();
                app.projects = library.projects().unwrap_or_default();
                app.tabs = tabs::State::load(&library);
                // `--open-session <id>`: a second window ("Open in new window") on one chat.
                let requested = secondary_session().and_then(|id| library.session(&id).ok());
                app.secondary = secondary_session().is_some();
                let active = if let Some(summary) = requested { Some(summary) } else if app.appearance.preferences.reopen_last_chat {
                    app.tabs.active.as_ref().and_then(|id| library.session(id).ok()).or_else(|| library.active_session().ok().flatten())
                } else { None };
                app.pets.load_active(library.root());
                app.library = Some(library);
                app.refresh_sessions();
                app.restore_gateway_chat(active);
            }
            Err(error) => app.error(format!("Could not open your chat library: {error:#}")),
        }
        app.models=models::State::open(&mut app.config);
        let mut initial = vec![widget::operation::focus(COMPOSER_ID), iced::system::theme().map(Message::SystemTheme)];
        if app.profile_settings.saved.auto_archive { initial.push(Task::done(Message::ProfileSetting(profile_settings::Action::Maintain))); }
        let voice_events = iced::futures::stream::unfold(app.voice.events(), |receiver| async move { receiver.recv().await.ok().map(|event| (event, receiver)) });
        initial.push(Task::run(voice_events, |(epoch, signal)| Message::Voice(voice::Action::Signal(epoch, signal))));
        if app.ready() {initial.push(Task::done(Message::Models(models::Action::Refresh)));}
        if !app.secondary { app.quick_entry.register(); }
        let (main_window, open) = iced::window::open(window_settings());
        app.webview.window = Some(main_window);
        initial.insert(0, open.map(|id| Message::WindowId(Some(id))));
        #[cfg(feature = "ui-test")]
        if std::env::var_os("HERMES_RUST_OPEN_ARTIFACT").is_some() {
            app.onboarding.open = false;
            initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_secs(2)).await; }, |_| Message::Navigate(Screen::Artifacts)));
        }
        let profile = app.library.as_ref().map(|library| library.root().to_path_buf());
        let workspace = app.workspace();
        // Background services belong to the first window only; a second one would run every job twice.
        if !app.secondary { initial.push(app.scheduled.reset(profile, workspace).map(Message::Scheduled)); }
        // The HUD hook also runs on screen (no capture), so the native frost and fade can be photographed.
        #[cfg(feature = "ui-test")]
        if std::env::var_os("HERMES_RUST_TEST_HUD").is_some() {
            app.onboarding.open = false;
            initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(600)).await; }, |_| Message::Hud(hud::Action::Toggle)));
            #[cfg(target_os = "macos")]
            if std::env::var_os("HERMES_RUST_TEST_HUD_INPUT").is_some() {
                initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(3000)).await; }, |_| Message::HudTestInput));
            }
        }
        #[cfg(feature = "ui-test")]
        if std::env::var_os("HERMES_RUST_TEST_CAPTURE").is_some() {
            if std::env::var_os("HERMES_RUST_TEST_PET").is_some() {
                app.onboarding.open = false;
                initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(500)).await; }, |_| Message::Overlay(overlays::Action::TogglePet)));
            }
            if std::env::var_os("HERMES_RUST_TEST_QUICK").is_some() {
                app.onboarding.open = false;
                initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(600)).await; }, |_| Message::QuickEntry(quick_entry::Action::Hotkey)));
                initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(1000)).await; }, |_| Message::QuickEntry(quick_entry::Action::Text("Summarize my unread mail".into()))));
            }
            initial.push(Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(1800)).await; }, |_| Message::Capture));
        }
        if app.backdrop { initial.push(Self::load_backdrop()); }
        if std::env::args().any(|argument| argument == "--sign-in") {
            app.config = app.portal_config();
            app.screen = Screen::Provider; app.settings_view = settings::View::Accounts;
            initial.push(Task::done(Message::PortalStart));
        }
        initial.append(&mut app.chat.tasks);
        (app, Task::batch(initial))
    }
    fn subscription(&self) -> Subscription<Message> {
        let maintain=if self.profile_settings.saved.auto_archive {iced::time::every(std::time::Duration::from_secs(3600)).map(|_|Message::ProfileSetting(profile_settings::Action::Maintain))}else{Subscription::none()};
        let memory=if self.statusbar.preferences.visible && self.statusbar.shows("system-resources") {iced::time::every(std::time::Duration::from_secs(3)).map(|_|Message::Statusbar(statusbar::Action::Memory(None)))}else{Subscription::none()};
        let clock=if self.statusbar.preferences.visible && ((self.running && self.statusbar.shows("running-timer")) || self.statusbar.shows("session-timer")) {iced::time::every(std::time::Duration::from_secs(1)).map(|_|Message::Statusbar(statusbar::Action::Tick))}else{Subscription::none()};
        let preview=if self.screen==Screen::Artifacts && self.artifacts.html_preview().is_some() {iced::time::every(std::time::Duration::from_millis(500)).map(|_|Message::WebviewTick)}else{Subscription::none()};
        // The main window's id, for the native preview webview (window::latest can run before it opens).
        let window=iced::window::open_events().map(|id|Message::WindowId(Some(id)));
        Subscription::batch([maintain,memory,clock,preview,window,iced::event::listen_with(notifications::focus),iced::event::listen_with(attachments::drop_event), iced::event::listen_with(palette_keyboard_event), iced::window::close_requests().map(Message::WindowClose), self.quick_entry.subscription(), if self.voice.preferences.wake_word && !self.voice.wake_listening { iced::time::every(std::time::Duration::from_secs(5)).map(|_| Message::Voice(voice::Action::WakeArm)) } else { Subscription::none() }, overlays::subscription(self), iced::event::listen_with(quick_entry_window_event), iced::system::theme_changes().map(Message::SystemTheme), self.scheduled.subscription().map(Message::Scheduled), sync::subscription(), self.hud.subscription().map(Message::Hud)])
    }
    fn error(&mut self, text: impl Into<String>) { self.banner = Some(Banner { text: text.into(), error: true }); }
    fn note(&mut self, text: impl Into<String>) { self.banner = Some(Banner { text: text.into(), error: false }); }
    fn show_memory_recall(&mut self, status: hermes_engine::RecallStatus) {
        if status.count == 0 {
            self.note(format!("{} recalled context", status.provider_label));
        } else {
            let noun = if status.count == 1 { "memory" } else { "memories" };
            self.note(format!("{} recalled {} {noun}", status.provider_label, status.count));
        }
    }
    fn is_portal(&self) -> bool { portal::is_portal(&self.config.base_url) }
    fn portal_config(&self) -> Config {
        Config { base_url: portal::INFERENCE.into(), model: if self.is_portal() { self.config.model.clone() } else { std::env::var("HERMES_RUST_MODEL").unwrap_or_else(|_| portal::AUTO_MODEL.into()) }, api_key: String::new() }
    }
    fn ready(&self) -> bool { self.config.validate().is_ok() && (!self.is_portal() || self.portal_connected) }
    fn workspace(&self) -> Option<PathBuf> {
        self.current.as_ref().and_then(|session| self.library.as_ref().and_then(|library| library.workspace_for_session(&session.id).ok().flatten())
            .or_else(|| Some(session.directory.join("workspace"))))
    }
    fn chat_project(&self) -> Option<&Project> {
        self.current.as_ref().and_then(|current| current.project_id.as_ref()).and_then(|id| self.projects.iter().find(|project| &project.id == id))
    }
    fn archive_scope(&self) -> Option<ArchiveScope> {
        let (session_id, project_id) = match &self.current {
            Some(session) => (Some(session.id.clone()), None),
            None => (None, self.project.as_ref().map(|project| project.id.clone())),
        };
        Some(ArchiveScope {
            profile_root: self.library.as_ref()?.root().to_owned(),
            session_id,
            project_id,
        })
    }
    fn load_backdrop() -> Task<Message> {
        Task::perform(async { tokio::task::spawn_blocking(backdrop::texture).await.map_err(|e|e.to_string()).and_then(|r|r) },Message::BackdropLoaded)
    }
    fn draft_timer(&self)->Task<Message> {
        let revision=self.draft_revision;
        Task::perform(async move { tokio::time::sleep(std::time::Duration::from_millis(180)).await; revision }, Message::DraftIdle)
    }
    fn save_draft(&self) -> Result<(), String> {
        if let Some(current) = &self.current {
            let draft = self.composer.text();
            let path = current.directory.join("draft.txt");
            if draft.trim().is_empty() {
                match std::fs::remove_file(path) { Ok(()) => {}, Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}, Err(e) => return Err(e.to_string()) }
            } else {
                let temp = current.directory.join("draft.tmp");
                std::fs::write(&temp, draft).map_err(|e| e.to_string())?;
                std::fs::rename(temp, path).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
    fn save_queue(&self) -> Result<(), String> {
        if self.group_round.is_none() { return Ok(()); }
        self.current.as_ref().map_or(Ok(()), |current| self.queue.save(&current.directory))
    }
    fn save_tabs(&mut self) {
        if self.secondary { return; }
        if self.connections.config.mode != connections::Mode::Local { return; }
        if let Some(library) = &self.library && let Err(error) = self.tabs.save(library.root()) { self.error(format!("Could not save session tabs: {error}")); }
    }
    fn clear_draft_files(&mut self) {
        if !self.draft_files.files.is_empty() && let Some(current) = &self.current
            && let Err(error) = self.draft_files.replace(&current.directory, vec![]) { self.error(error); }
    }
    fn queue_from_composer(&mut self, front: bool) -> bool {
        let prompt = self.composer.text().trim().to_owned();
        if self.draft_files.pending > 0 { return false; }
        let id = match self.queue.push(prompt, self.draft_files.files.clone(), front) { Ok(id) => id, Err(error) => { self.error(error); return false; } };
        if let Err(error) = self.save_queue() {
            self.queue.entries.retain(|entry| entry.id != id); self.error(format!("Could not save queued message: {error}")); return false;
        }
        self.clear_draft_files();
        self.composer = text_editor::Content::new();
        if let Err(error) = self.save_draft() { self.error(format!("Message queued, but draft could not be cleared: {error}")); }
        true
    }
    fn cancel_response(&mut self) {
        if let Some(cancel) = &self.cancellation { cancel.cancel(); }
        for approval in self.approvals.drain(..) { if let Some(answer) = approval.answer { let _ = answer.try_send(false); } }
        self.stopping = self.running;
    }
    fn rebuild_markdown(&mut self) {
        self.rendered = self.history.turns.iter().map(|turn| turn.entries.iter().map(|entry| match entry {
            Entry::Assistant { text, .. } => Some(markdown::Content::parse(text)), _ => None,
        }).collect()).collect();
    }
    // Published engine snapshots only append to this turn. Session changes go
    // through open_chat and rebuild the full cache once.
    fn accept_snapshot(&mut self,history:History) {
        let previous=&self.history.turns;
        let next=&history.turns;
        let first=previous.len().saturating_sub(1).min(next.len());
        for (turn_index,turn) in next.iter().enumerate().skip(first) {
            if self.rendered.len()<=turn_index {self.rendered.push(Vec::new());}
            let items=&mut self.rendered[turn_index];
            for (entry_index,entry) in turn.entries.iter().enumerate(){
                let old=previous.get(turn_index).and_then(|turn|turn.entries.get(entry_index));
                if items.len()<=entry_index {items.push(None);}
                let slot=&mut items[entry_index];
                match (entry,old,slot.as_mut()){
                    (Entry::Assistant{text,..},Some(Entry::Assistant{text:old,..}),Some(content))=>{
                        if let Some(suffix)=text.strip_prefix(old){if !suffix.is_empty(){content.push_str(suffix);}}
                        else{*slot=Some(markdown::Content::parse(text));}
                    }
                    (Entry::Assistant{text,..},_,_)=>*slot=Some(markdown::Content::parse(text)),
                    _=>*slot=None,
                }
            }
            items.truncate(turn.entries.len());
        }
        self.rendered.truncate(next.len());
        self.history=history;
    }
    fn refresh_sessions(&mut self) {
        if self.connections.config.mode != connections::Mode::Local {
            self.list_gateway_chats();
            return;
        }
        if let Some(library) = &self.library {
            match library.list_sessions(&SessionQuery { project_id: self.project.as_ref().map(|p| p.id.clone()), archive: self.archive_filter, pinned: None, search: self.search.clone(), hide_empty: true }) {
                Ok(sessions) => self.sessions = sessions,
                Err(error) => self.error(format!("Could not list chats: {error:#}")),
            }
        }
        for summary in &self.sessions { self.tabs.summary(summary); }
        if let Some(current) = &self.current { self.tabs.summary(current); }
        if matches!(self.screen, Screen::CommandCenter | Screen::Agents) { self.refresh_command_center(); }
    }
    fn refresh_command_center(&mut self) {
        let result = self.library.as_ref().ok_or_else(|| "Local library is unavailable".to_owned())
            .and_then(|library| library.list_sessions(&SessionQuery { archive: ArchiveFilter::All, ..Default::default() }).map_err(|error| format!("Could not list local sessions: {error:#}")));
        match result {
            Ok(sessions) => { self.command_sessions = sessions; self.command_error = None; }
            Err(error) => self.command_error = Some(error),
        }
    }
    fn open_chat(&mut self, summary: SessionSummary) {
        self.open_gateway_chat(summary);
    }
    fn create_chat(&mut self) {
        if self.running { return; }
        if self.connections.config.mode != connections::Mode::Local { self.create_gateway_chat(); return; }
        if self.draft_files.pending > 0 { self.note("Wait for the attachment to finish before opening another tab."); return; }
        self.voice.stop_all();
        if self.current.as_ref().is_some_and(|summary| draft_surface(summary, &self.history))
            && self.composer.text().trim().is_empty() && self.draft_files.files.is_empty()
            && self.draft_files.pending == 0 && self.queue.entries.is_empty()
            && self.current.as_ref().is_some_and(|current| current.project_id.as_deref() == self.project.as_ref().map(|project| project.id.as_str())) {
            self.fresh_draft = true; self.screen = Screen::Chat;
            if let Some(current) = &self.current { self.tabs.activate(current, true); } self.save_tabs(); return;
        }
        let Some(library) = self.library.clone() else { return; };
        let project = self.project.as_ref().map(|project| project.id.clone());
        let reusable = self.tabs.reusable(&library, project.as_deref()).or_else(|| {
            library.sessions(project.as_deref()).ok().and_then(|sessions| sessions.into_iter().find(tabs::pristine))
        });
        if let Some(summary) = reusable {
            let id = summary.id.clone(); self.open_chat(summary);
            if self.current.as_ref().is_some_and(|current| current.id == id) { return; }
        }
        self.create_gateway_chat();
    }
    fn sync_workspace_memory(&self, root: PathBuf) -> Task<Message> {
        if !hermes_gateway::stock_plugins::has_memory_plugin() {
            return Task::none();
        }
        Task::perform(
            local_gateway::catalog_request(root, self.config.clone(), "plugins.memory.sync", serde_json::json!({})),
            Message::WorkspaceMemorySynced,
        )
    }
    fn send(&mut self) -> Task<Message> {
        let prompt = self.composer.text().trim().to_owned();
        if let Some(task) = slash::run(self, &prompt) { return task; }
        if self.draft_files.pending>0 || (prompt.is_empty() && self.draft_files.files.is_empty()) { return Task::none(); }
        if self.running && self.group_round.is_none() { return self.submit_gateway(prompt); }
        if self.running {
            if self.queue_from_composer(true) {
                self.queue.parked = false; self.redirecting = true;
                if let Err(error) = self.save_queue() { self.queue.parked = true; self.redirecting = false; self.error(error); return Task::none(); }
                self.cancel_response();
            }
            return Task::none();
        }
        self.submit_gateway(prompt)
    }
    fn palette_context(&self) -> commands::Context {
        commands::Context { running: self.running, composer_has_text: !self.composer.text().trim().is_empty(), queued: !self.queue.entries.is_empty(), terminal_visible: self.right_pane == Some(RightPane::Terminal) }
    }
    fn refresh_palette_context(&mut self) {
        let context = self.palette_context();
        self.commands.set_context(context);
        self.commands.set_bindings(self.shortcuts.palette_bindings());
    }
    fn palette_command(&mut self, command: commands::Command) -> Task<Message> {
        use commands::Command;
        match command {
            Command::NewSession => self.update(Message::NewChat),
            Command::Settings => self.update(Message::Navigate(Screen::Provider)),
            Command::Capabilities => self.update(Message::Navigate(Screen::Tools)),
            Command::Artifacts => self.update(Message::Navigate(Screen::Artifacts)),
            Command::OpenFolder => { self.screen = Screen::Workspaces; self.palette_open_folder = true; self.update(Message::BrowseWorkspace) }
            Command::SessionsImport => self.update(Message::Navigate(Screen::Import)),
            Command::Models => self.update(Message::SettingsView(settings::View::Models)),
            Command::Accounts => self.update(Message::SettingsView(settings::View::Accounts)),
            Command::About => self.update(Message::Navigate(Screen::About)),
            Command::ToggleSidebar => self.update(Message::ToggleSidebar),
            Command::ToggleStatusbar=>self.update(Message::Statusbar(statusbar::Action::ToggleVisible)),
            Command::ToggleHud=>self.update(Message::Hud(hud::Action::Toggle)),
            Command::TogglePet=>self.update(Message::Overlay(overlays::Action::TogglePet)),
            Command::PetGallery=>{ let open = self.update(Message::Navigate(Screen::Pets)); open }
            Command::FlipPanes => self.update(Message::FlipPanes),
            Command::Files => self.update(Message::Navigate(Screen::Files)),
            Command::Review => self.update(Message::Navigate(Screen::Git)),
            Command::Terminal => self.update(Message::Navigate(Screen::Terminal)),
            Command::NewTerminal => { self.screen = Screen::Chat; self.right_pane = Some(RightPane::Terminal); self.sync_terminal_context(); self.terminal.update(terminal::Action::New).map(Message::Terminal) }
            Command::Stop => self.update(Message::Stop),
            Command::QueueCurrent => self.update(Message::QueueCurrent),
            Command::ResumeQueue => self.update(Message::QueueResume),
            Command::Shortcuts => self.update(Message::SettingsView(settings::View::Shortcuts)),
            Command::Welcome => { self.onboarding.open = true; Task::none() }
        }
    }
    fn tab_action(&mut self, action: tabs::Action) -> Task<Message> {
        use tabs::Action;
        let pressed = matches!(&action, Action::Press(_));
        match action {
            Action::Press(id) | Action::Select(id) => {
                if pressed && self.modifiers.command() { return self.tab_action(Action::Close(id)); }
                if self.current.as_ref().is_some_and(|current| current.id == id) {
                    self.screen = Screen::Chat; self.tabs.active = Some(id.clone()); self.tabs.menu = None;
                    self.fresh_draft = self.current.as_ref().is_some_and(|summary| draft_surface(summary, &self.history));
                } else {
                    if self.running { self.note("Stop the current response before switching session tabs."); return Task::none(); }
                    if self.draft_files.pending > 0 { self.note("Wait for the attachment to finish before switching tabs."); return Task::none(); }
                    let summary = self.library.as_ref().and_then(|library| library.session(&id).ok());
                    if let Some(summary) = summary { self.open_chat(summary); } else { self.error("This session is no longer in the local library."); return Task::none(); }
                }
                if pressed { self.tabs.dragging = Some(id.clone()); }
                self.save_tabs();
                let index = self.tabs.entries.iter().position(|tab| tab.id == id).unwrap_or(0);
                let offset: f32 = self.tabs.entries.iter().take(index).map(tabs::tab_width).sum();
                return Task::batch([widget::operation::focus(COMPOSER_ID), widget::operation::snap_to_end(SCROLL_ID),
                    widget::operation::scroll_to("hermes-session-tabs", widget::operation::AbsoluteOffset { x: Some(offset), y: None })]);
            }
            Action::Close(id) => {
                let current = self.current.as_ref().is_some_and(|current| current.id == id);
                if current && (self.running || self.draft_files.pending > 0) { self.note("This session is still working. Stop the response before closing its tab."); return Task::none(); }
                if current && let Err(error) = self.save_draft().and_then(|_| self.save_queue()) { self.error(error); return Task::none(); }
                let previous = self.tabs.clone();
                let next = self.tabs.close(&id);
                if current {
                    if let Some(next) = next {
                        if let Some(summary) = self.library.as_ref().and_then(|library| library.session(&next).ok()) {
                            self.open_chat(summary);
                            if self.current.as_ref().is_none_or(|current| current.id != next) { self.tabs = previous; return Task::none(); }
                        } else { self.tabs = previous; self.error("Could not open the next session tab."); return Task::none(); }
                    } else {
                        self.current = None; self.session = None; self.history = History::default(); self.rebuild_markdown();
                        self.composer = text_editor::Content::new(); self.queue = queue::PromptQueue::default(); self.queue_edit = None;
                        self.create_chat();
                    }
                }
                self.save_tabs();
            }
            Action::CloseOthers(id) => {
                if !self.tabs.entries.iter().any(|tab| tab.id == id) { return Task::none(); }
                if self.current.as_ref().is_none_or(|current| current.id != id) {
                    let task = self.tab_action(Action::Select(id.clone()));
                    if self.current.as_ref().is_none_or(|current| current.id != id) { return task; }
                }
                let others: Vec<_> = self.tabs.entries.iter().filter(|tab| tab.id != id).map(|tab| tab.id.clone()).collect();
                for other in others { self.tabs.close(&other); } self.tabs.active = Some(id); self.save_tabs();
            }
            Action::CloseToRight(id) => {
                if let Some(index) = self.tabs.entries.iter().position(|tab| tab.id == id) {
                    let right: Vec<_> = self.tabs.entries.iter().skip(index + 1).map(|tab| tab.id.clone()).collect();
                    if self.current.as_ref().is_some_and(|current| right.contains(&current.id)) {
                        let task = self.tab_action(Action::Select(id.clone()));
                        if self.current.as_ref().is_none_or(|current| current.id != id) { return task; }
                    }
                    for other in right { self.tabs.close(&other); } self.save_tabs();
                }
            }
            Action::CloseAll => {
                if self.running || self.draft_files.pending > 0 { self.note("Stop the current response before closing all session tabs."); return Task::none(); }
                if let Err(error) = self.save_draft().and_then(|_| self.save_queue()) { self.error(error); return Task::none(); }
                let ids: Vec<_> = self.tabs.entries.iter().map(|tab| tab.id.clone()).collect();
                for id in ids { self.tabs.close(&id); }
                self.current = None; self.session = None; self.history = History::default(); self.rebuild_markdown();
                self.composer = text_editor::Content::new(); self.queue = queue::PromptQueue::default(); self.queue_edit = None;
                self.create_chat(); self.save_tabs();
            }
            Action::Reopen => {
                if self.running { self.note("Stop the current response before reopening a session tab."); return Task::none(); }
                while let Some(id) = self.tabs.reopen() {
                    if let Some(summary) = self.library.as_ref().and_then(|library| library.session(&id).ok()) { self.open_chat(summary); break; }
                }
                self.save_tabs();
            }
            Action::Next(forward) => { if let Some(id) = self.tabs.next(forward) { return self.tab_action(Action::Select(id)); } }
            Action::Menu(id) => self.tabs.menu = if self.tabs.menu.as_ref() == Some(&id) { None } else { Some(id) },
            Action::Hover(id) => {
                if let (Some(dragged), Some(target)) = (self.tabs.dragging.clone(), &id)
                    && self.tabs.hover.as_ref() != Some(target) && self.tabs.move_to(&dragged, target) { self.save_tabs(); }
                self.tabs.hover = id;
            }
            Action::Release => self.tabs.dragging = None,
            Action::Dismiss => self.tabs.menu = None,
        }
        Task::none()
    }
    fn dispatch_keyboard(&mut self, event: keyboard::Event, status: iced::event::Status) -> Task<Message> {
        self.modifiers = match &event { keyboard::Event::KeyPressed { modifiers, .. } | keyboard::Event::KeyReleased { modifiers, .. } | keyboard::Event::ModifiersChanged(modifiers) => *modifiers };
        if self.shortcuts.is_capturing() { return self.shortcuts.update(shortcuts::Message::Capture(event)).map(Message::Shortcuts); }
        if self.vault.has_dialog() && matches!(&event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), .. }) { return vault::update(self, vault::Message::Cancel); }
        if (self.voice.recording || self.voice.authorizing || self.voice.finishing) && matches!(&event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), .. }) { return self.voice_update(voice::Action::CancelRecord); }
        if let Some(task)=reasoning::keyboard(self,&event){return task;}
        if self.models.picker && matches!(&event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Escape), .. }) { self.models.picker = false; return Task::none(); }
        let action = self.shortcuts.bindings().action_for_event(&event, status == iced::event::Status::Captured);
        if self.commands.is_open() {
            if action == Some(shortcuts::Action::Palette) { return self.update(Message::OpenCommands); }
            if let Some(message) = commands::key_message(&event) {
                if !matches!(message, commands::Message::Activate) || status == iced::event::Status::Ignored { return self.update(Message::Commands(message)); }
            }
            return Task::none();
        }
        if let Some(action) = action {
            use shortcuts::Action;
            return match action {
                Action::Palette => self.update(Message::OpenCommands),
                Action::Shortcuts => self.update(Message::SettingsView(settings::View::Shortcuts)),
                Action::Models => self.update(Message::Models(models::Action::Open)),
                Action::NewSession => self.update(Message::NewChat),
                Action::NewTab => self.update(Message::NewChat),
                Action::NextSession => self.tab_action(tabs::Action::Next(true)),
                Action::PreviousSession => self.tab_action(tabs::Action::Next(false)),
                Action::CloseTab => self.current.as_ref().map(|current| current.id.clone()).map(|id| self.tab_action(tabs::Action::Close(id))).unwrap_or_else(Task::none),
                Action::ReopenTab => self.tab_action(tabs::Action::Reopen),
                Action::Settings => self.update(Message::Navigate(Screen::Provider)),
                Action::Capabilities => self.update(Message::Navigate(Screen::Tools)),
                Action::Artifacts => self.update(Message::Navigate(Screen::Artifacts)),
                Action::Sidebar => self.update(Message::ToggleSidebar),
                Action::Review => self.update(Message::Navigate(Screen::Git)),
                Action::Files => self.update(Message::Navigate(Screen::Files)),
                Action::Terminal => self.update(Message::Navigate(Screen::Terminal)),
                Action::NewTerminal => self.palette_command(commands::Command::NewTerminal),
                Action::ToggleStatusbar=>self.update(Message::Statusbar(statusbar::Action::ToggleVisible)),
                Action::Hud=>self.update(Message::Hud(hud::Action::Toggle)),
            Action::FlipPanes => self.update(Message::FlipPanes),
                Action::OpenFolder => self.palette_command(commands::Command::OpenFolder),
                Action::PinSession => self.current.as_ref().map(|current| current.id.clone()).map(|id| self.update(Message::TogglePin(id))).unwrap_or_else(Task::none),
                Action::ArchiveSession => self.current.as_ref().map(|current| (current.id.clone(), !current.archived)).map(|(id, archived)| self.update(Message::ArchiveSession(id, archived))).unwrap_or_else(Task::none),
                Action::SearchSessions => { self.sidebar_open = true; widget::operation::focus("hermes-session-search") }
                Action::FocusComposer => {
                    if self.screen != Screen::Chat || self.layout_menu || self.attach_menu { return Task::none(); }
                    if let keyboard::Event::KeyPressed { key: keyboard::Key::Character(character), modifiers, .. } = &event
                        && character.as_str() == "/" && modifiers.is_empty() {
                        self.composer.perform(text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("/".into()))));
                        if let Err(error) = self.save_draft() { self.error(error); }
                    }
                    widget::operation::focus(COMPOSER_ID)
                }
            };
        }
        if status == iced::event::Status::Ignored && let keyboard::Event::KeyPressed { key, modifiers, .. } = event {
            if key == keyboard::Key::Named(keyboard::key::Named::Escape) {
                if self.running { return self.update(Message::Stop); }
                self.screen = Screen::Chat; self.layout_menu = false; self.attach_menu = false;
            }
            if modifiers.command() && modifiers.shift() && matches!(key.as_ref(), keyboard::Key::Character("k" | "K")) { return self.update(Message::QueueResume); }
        }
        Task::none()
    }
    /// Every update, then keep the native HTML preview in line with the UI.
    fn update_synced(&mut self, message: Message) -> Task<Message> {
        let bounds = matches!(message, Message::WebviewBounds(_) | Message::WindowId(_));
        let task = self.update(message);
        if bounds { return task; }
        let wake = overlays::sync_wake(self);
        Task::batch([task, self.sync_webview(), wake])
    }
    fn sync_webview(&mut self) -> Task<Message> {
        // Hidden unless the Artifacts preview is on screen and nothing covers it.
        let covered = self.commands.is_open() || self.models.picker || self.onboarding.open || self.statusbar.menu.is_some()
            || self.layout_menu || self.attach_menu || self.reasoning.anchor.is_some();
        let file = (self.screen == Screen::Artifacts && !covered).then(|| self.artifacts.html_preview()).flatten();
        #[cfg(debug_assertions)]
        if std::env::var_os("HERMES_RUST_WEBVIEW_TRACE").is_some() {
            eprintln!("cover: screen={:?} commands={} picker={} onboarding={} statusbar={} layout={} attach={} reasoning={} html={:?}", self.screen, self.commands.is_open(), self.models.picker,
                self.onboarding.open, self.statusbar.menu.is_some(), self.layout_menu, self.attach_menu, self.reasoning.anchor.is_some(), self.artifacts.trace());
        }
        let background = ui::palette(self.dark).background;
        let scale = self.appearance.preferences.ui_scale as f32 / 100.0;
        self.webview.sync(file, background, scale, Message::WebviewBounds)
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        let alert=notifications::on_event(self,&message);
        if self.hud.on && matches!(&message, Message::Edit(_) | Message::Send | Message::Engine(..)) { self.hud.touch(); }
        if matches!(&message,Message::Models(models::Action::Disconnected(_))|Message::PortalStart){self.billing.clear();}
        let work = self.update_inner(message);
        let queued = std::mem::take(&mut self.chat.tasks);
        Task::batch([alert, work, Task::batch(queued)])
    }
    fn update_inner(&mut self, message: Message) -> Task<Message> {
        let task = self.update_message(message);
        if self.screen != Screen::Provider || self.settings_view != settings::View::Vault { self.vault.dismiss(); }
        if self.commands.is_open() { self.refresh_palette_context(); }
        task
    }
    fn update_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tab(action) => return self.tab_action(action),
            Message::GlobalKey(event, status) => return self.dispatch_keyboard(event, status),
            Message::FocusCommands => return if self.commands.is_open() { self.commands.focus().map(Message::Commands) } else { Task::none() },
            Message::Shortcuts(message) => return self.shortcuts.update(message).map(Message::Shortcuts),
            Message::Vault(message) => return vault::update(self, message),
            Message::OpenCommands => {
                if self.commands.is_open() { self.commands.close(); return widget::operation::focus(COMPOSER_ID); }
                self.models.picker = false;
                self.layout_menu = false; self.attach_menu = false; self.session_menu = None; self.session_filter_menu = false;
                self.refresh_palette_context();
                return self.commands.open().map(Message::Commands);
            }
            Message::Commands(message) => {
                self.refresh_palette_context();
                let was_open = self.commands.is_open();
                let (task, action) = self.commands.update(message);
                let effect = action.map(|command| self.palette_command(command)).unwrap_or_else(Task::none);
                let restore_composer = was_open && !self.commands.is_open() && action.is_none_or(|command| matches!(command, commands::Command::ToggleSidebar | commands::Command::FlipPanes | commands::Command::Files | commands::Command::Review | commands::Command::Stop | commands::Command::QueueCurrent | commands::Command::ResumeQueue));
                let work = Task::batch([task.map(Message::Commands), effect, if restore_composer { widget::operation::focus(COMPOSER_ID) } else { Task::none() }]);
                return if self.commands.is_open() { Task::batch([work, Task::perform(async { tokio::task::yield_now().await; }, |_| Message::FocusCommands)]) } else { work };
            }
            Message::Edit(_) if self.reasoning.anchor.is_some()=>return Task::none(),
            Message::Edit(action) => {
                let was_slash = self.composer.text().trim_start().starts_with('/');
                let changed = action.is_edit(); self.composer.perform(action);
                if changed {
                    self.slash.index = 0;
                    let entered_slash = !was_slash && self.composer.text().trim_start().starts_with('/');
                    self.draft_revision=self.draft_revision.wrapping_add(1);
                    if !self.draft_save_pending { self.draft_save_pending=true; return Task::batch([self.draft_timer(), if entered_slash { slash::refresh(self) } else { Task::none() }]); }
                    if entered_slash { return slash::refresh(self); }
                }
            }
            Message::IntroSize(size) => self.intro_width=size.width,
            Message::ToggleBackdrop => {
                let new=!self.backdrop;
                let saved=Session::default_dir().map_err(|e|e.to_string()).and_then(|dir|backdrop::save(&dir,new));
                if let Err(error)=saved { self.error(error); return Task::none(); }
                self.backdrop=new;
                if new && self.backdrop_image.is_none(){return Self::load_backdrop();}
            }
            Message::BackdropLoaded(result)=>match result { Ok(image)=>self.backdrop_image=Some(image),Err(e)=>self.error(e) },
            Message::DraftIdle(revision)=>{
                if revision!=self.draft_revision { return self.draft_timer(); }
                self.draft_save_pending=false;
                if let Err(error)=self.save_draft(){self.error(format!("Could not save your draft: {error}"));}
            }
            Message::Attachment(action) => return self.attachment_action(action),
            Message::Send => return self.send(),
            Message::Gateway(epoch, update) => { if epoch == self.run_id { return self.gateway_update(update); } }
            Message::GatewayQueue(sid, id, resume) => return self.change_pending(sid, id, resume),
            Message::Stop => {
                if self.group_round.is_none() { return self.interrupt_gateway(); }
                self.queue.parked = true; self.redirecting = false;
                if let Err(error) = self.save_queue() { self.error(error); }
                self.cancel_response();
            }
            Message::Engine(id, event) if id == self.run_id && self.running => {
                match event {
                    Event::Snapshot(history) => {
                        self.approvals.retain(|a|a.answer.as_ref().is_none_or(|tx| !tx.is_closed()));
                        self.accept_snapshot(history);
                        if self.queue.admitted(&self.history) && let Err(error) = self.save_queue() { self.error(error); }
                    }
                    Event::Text(delta) => {
                        self.statusbar.streamed(delta.len());
                        if let Some(turn) = self.history.turns.last_mut()
                            && let Some(Entry::Assistant { text, .. }) = turn.entries.last_mut() {
                            text.push_str(&delta);
                            if let Some(items) = self.rendered.last_mut()
                                && let Some(Some(content)) = items.last_mut() { content.push_str(&delta); }
                        }
                    }
                    Event::Approval { call, answer } => self.approvals.push(Approval { call, generation: id, answer: Some(answer) }),
                    Event::MemoryRecall(status) => self.show_memory_recall(status),
                    Event::Subagent(progress) => {
                        let list = &mut self.subagents;
                        match list.iter_mut().find(|p| p.id == progress.id) { Some(slot) => *slot = progress, None => list.push(progress) }
                        return Task::none();
                    }
                }
                if self.follow_stream { return widget::operation::snap_to_end(SCROLL_ID); }
            }
            Message::Finished(id, history, result) if id == self.run_id => {
                self.accept_snapshot(history); self.running = false; self.stopping = false; self.statusbar.turn_finished();
                self.cancellation = None; self.approvals.clear();
                if let Err(error) = result { self.error(error); }
                self.queue.settle(&self.history);
                if self.history.turns.last().is_some_and(|turn| matches!(turn.status, hermes_engine::Status::Failed(_))) ||
                    (!self.redirecting && self.history.turns.last().is_some_and(|turn| turn.status == hermes_engine::Status::Interrupted)) {
                    self.queue.parked = true;
                }
                self.redirecting = false;
                if let Err(error) = self.save_queue() { self.queue.parked = true; self.error(error); }
                if let Some(library) = &self.library && let Some(current) = &self.current {
                    match library.touch_session(&current.id) {
                        Ok(updated) => self.current = Some(updated),
                        Err(error) => self.error(format!("Could not update the chat library: {error:#}")),
                    }
                }
                self.refresh_sessions();
                if self.closing && !self.scheduled.has_running_jobs() { return iced::exit(); }

                if (self.voice.preferences.auto_read || self.voice.conversation) && self.voice.speech_unavailable().is_none() && !self.voice.recording && !self.voice.authorizing && !self.voice.finishing
                    && let Some((turn_index, turn))=self.history.turns.iter().enumerate().next_back()
                    && turn.status==hermes_engine::Status::Complete
                    && let Some((entry_index,Entry::Assistant{text,..}))=turn.entries.iter().enumerate().rev().find(|(_,entry)|matches!(entry,Entry::Assistant{text,..} if !text.trim().is_empty())) {
                    let speech=self.voice_update(voice::Action::Speak(format!("{turn_index}:{entry_index}"),text.clone()));
                    return Task::batch([speech,if self.follow_stream {widget::operation::snap_to_end(SCROLL_ID)} else {Task::none()}]);
                }
                if self.follow_stream { return widget::operation::snap_to_end(SCROLL_ID); }
            }
            Message::Engine(..) | Message::Finished(..) => {},
            Message::Approve(prompt, generation, approved) => {
                if self.group_round.is_none() { return self.answer_gateway(prompt, generation, approved); }
                if let Some(index) = self.approvals.iter().position(|a| a.call.id == prompt && a.generation == generation)
                    && let Some(answer) = self.approvals.remove(index).answer { let _ = answer.try_send(approved); }
            }
            Message::NewChat if !self.running => { self.banner = None; self.create_chat(); return widget::operation::focus(COMPOSER_ID); }
            Message::SelectChat(id) => {
                if self.modifiers.shift() { return self.update(Message::TogglePin(id)); }
                self.fresh_draft = false;
                self.session_menu = None;
                if self.current.as_ref().is_some_and(|current| current.id == id) {
                    self.screen = Screen::Chat;
                    self.fresh_draft = self.current.as_ref().is_some_and(|summary| draft_surface(summary, &self.history));
                }
                else if self.group_round.is_none() && let Some(summary) = self.find_chat(&id) {
                    self.banner = None; self.open_chat(summary);
                }
                return widget::operation::snap_to_end(SCROLL_ID);
            }
            Message::NewChat => {},
            Message::Search(value) => { self.search = value; self.refresh_sessions(); self.list_gateway_chats(); }
            Message::CommandSearch(value) => self.command_search = value,
            Message::CommandSource(source) => self.command_source = source,
            Message::CommandRefresh => self.refresh_command_center(),
            Message::CommandOpenSession(id) => {
                if self.current.as_ref().is_some_and(|current| current.id == id) { self.screen = Screen::Chat; }
                else if self.running { self.command_error = Some("Stop the current response before switching sessions.".into()); }
                else if let Some(library) = &self.library {
                    match library.session(&id) {
                        Ok(summary) => { self.banner = None; self.open_chat(summary); }
                        Err(error) => self.command_error = Some(format!("Could not open that session: {error:#}")),
                    }
                }
                return widget::operation::snap_to_end(SCROLL_ID);
            }
            Message::SessionMenu(id) => { self.session_menu = if self.session_menu.as_ref() == Some(&id) { None } else { Some(id) }; self.session_filter_menu = false; }
            Message::ToggleSessionFilter => { self.session_filter_menu = !self.session_filter_menu; self.session_menu = None; }
            Message::ArchiveFilter(filter) => { self.archive_filter = filter; self.session_filter_menu = false; self.refresh_sessions(); }
            Message::TogglePin(id) => {
                if let Some(summary) = self.find_chat(&id) {
                    if summary.source == SessionSource::Native {
                        match self.set_gateway_pin(id.clone(), !summary.pinned) {
                            Ok(task) => return task,
                            Err(error) => self.error(error),
                        }
                    } else if let Some(library) = &self.library {
                        match library.set_pinned(&id, !summary.pinned) {
                            Ok(updated) => {
                                if self.current.as_ref().is_some_and(|current| current.id == id) { self.current = Some(updated); }
                                self.session_menu = None; self.refresh_sessions();
                            }
                            Err(error) => self.error(format!("Could not update session pin: {error:#}")),
                        }
                    }
                }
            }
            Message::CatalogPinned(id, pinned, result) => match result {
                Ok(_) => {
                    if let Some(current) = self.current.as_mut().filter(|current| current.id == id) {
                        current.pinned = pinned;
                    }
                    self.session_menu = None;
                    self.refresh_sessions();
                }
                Err(error) => self.error(format!("Could not update session pin: {error}")),
            },
            Message::ArchiveSession(id, archived) => {
                let is_current = self.current.as_ref().is_some_and(|current| current.id == id);
                if self.running && is_current { self.note("Stop the current response before archiving this session."); return Task::none(); }
                if is_current && let Err(error) = self.save_draft().and_then(|_| self.save_queue()) { self.error(error); return Task::none(); }
                let summary = self.find_chat(&id).or_else(|| self.library.as_ref().and_then(|library| library.session(&id).ok()));
                if summary.as_ref().is_some_and(|summary| summary.source == SessionSource::Native) {
                    match self.set_gateway_archive(id.clone(), archived) {
                        Ok(task) => return task,
                        Err(error) => self.error(error),
                    }
                    return Task::none();
                }
                if let Some(library) = &self.library {
                    match library.set_archived(&id, archived) {
                        Ok(_) => {
                            self.session_menu = None;
                            if archived && is_current {
                                self.current = None; self.session = None; self.history = History::default(); self.rebuild_markdown();
                                self.queue = queue::PromptQueue::default(); self.queue_edit = None; self.composer = text_editor::Content::new();
                                self.archive_filter = ArchiveFilter::Active; self.search.clear(); self.create_chat();
                            }
                            self.refresh_sessions();
                        }
                        Err(error) => self.error(format!("Could not archive session: {error:#}")),
                    }
                }
            }
            Message::CatalogSessionChanged(id, archived, result) => match result {
                Ok(_) => {
                    let is_current = self.current.as_ref().is_some_and(|current| current.id == id);
                    self.session_menu = None;
                    if archived && is_current {
                        self.current = None; self.session = None; self.history = History::default(); self.rebuild_markdown();
                        self.queue = queue::PromptQueue::default(); self.queue_edit = None; self.composer = text_editor::Content::new();
                        self.archive_filter = ArchiveFilter::Active; self.search.clear(); self.create_chat();
                    } else if !archived && is_current {
                        if let Some(current) = &mut self.current { current.archived = false; }
                    }
                    self.refresh_sessions();
                }
                Err(error) => self.error(format!("Could not archive session: {error}")),
            },
            Message::RenameFor(id) => {
                if !self.running && let Some(summary) = self.find_chat(&id) {
                    self.open_chat(summary); self.rename = self.current.as_ref().map(|summary| summary.title.clone()); self.session_menu = None;
                }
            }
            Message::Navigate(Screen::About)=>return self.update(Message::SettingsView(settings::View::About)),
            Message::Navigate(screen) if self.hud.on && screen != Screen::Chat => {
                // A full page needs the app window: leave the HUD, then go there.
                let window = self.webview.window; let busy = self.running || !self.approvals.is_empty();
                let leave = self.hud.update(window, busy, hud::Action::Toggle, Message::Hud);
                return Task::batch([leave, Task::done(Message::Navigate(screen))]);
            }
            Message::Navigate(screen) => {
                self.shortcuts.cancel_capture();
                if screen==Screen::Provider { self.portal_connected=portal::is_available(); }
                self.rename = None; self.attach_menu = false; self.layout_menu = false;
                if screen == Screen::Webhooks { let _ = webhooks::update(self, webhooks::Action::Refresh); }
                if screen == Screen::Profiles { let _ = profiles::update(self, profiles::Action::Refresh); }
                if screen == Screen::Pets { let _ = pets::update(self, pets::Action::Open); }
                if screen == Screen::Starmap { let _ = starmap::update(self, starmap::Action::Refresh); }
                if screen == Screen::Files {
                    self.screen = Screen::Chat;
                    if self.right_pane == Some(RightPane::Files) { self.right_pane = None; return Task::none(); }
                    self.right_pane = Some(RightPane::Files);
                    return self.files.reset(self.workspace()).map(Message::Files);
                }
                if screen == Screen::Git {
                    self.screen = Screen::Chat;
                    if self.right_pane == Some(RightPane::Review) { self.right_pane = None; return Task::none(); }
                    self.right_pane = Some(RightPane::Review);
                    let _ = self.git.reset(self.workspace());
                    return self.git.update(git::GitMessage::Refresh).map(Message::Git);
                }
                if screen == Screen::Terminal {
                    self.screen = Screen::Chat;
                    self.right_pane = if self.right_pane == Some(RightPane::Terminal) { None } else { Some(RightPane::Terminal) };
                    self.sync_terminal_context();
                    if self.right_pane.is_some() { return self.terminal.ensure_shell().map(Message::Terminal); }
                    return Task::none();
                }
                self.screen = screen;
                if screen == Screen::Workspaces { return self.project_discovery.refresh(self.projects.clone(), false).map(Message::ProjectDiscovery); }
                if screen==Screen::Tools{return capabilities::open(self);}
                if screen == Screen::Messaging { return self.messaging.load(self.library.clone()).map(Message::Messaging); }
                if screen == Screen::Cron {
                    let profile = self.library.as_ref().map(|library| library.root().to_path_buf());
                    let workspace = self.workspace();
                    return Task::batch([self.scheduled.reset(profile, workspace).map(Message::Scheduled), self.scheduled.update(scheduled::Action::Refresh, self.config.clone()).map(Message::Scheduled)]);
                }
                if matches!(screen, Screen::CommandCenter | Screen::Agents) { self.refresh_command_center(); }
                if screen == Screen::Artifacts {
                    return self.artifacts.refresh(self.library.clone(), self.current.clone().map(|summary| (summary, self.history.clone()))).map(Message::Artifacts);
                }
            }
            Message::ChoosePortal(portal) if !self.running => {
                return self.models_update(models::Action::Preset(if portal {models::Preset::Nous} else {models::Preset::OpenRouter}));
            }
            Message::BaseUrl(value) if !self.running => { if value != self.config.base_url { self.config.api_key.clear();self.models.selected_endpoint=None;self.models.invalidate(); } self.config.base_url = value;self.models.load_options(&self.config); }
            Message::Model(value) if !self.running => {self.config.model = value;self.models.load_options(&self.config);},
            Message::ApiKey(value) if !self.running => {self.config.api_key = value;self.models.invalidate();},
            Message::Models(action)=>return self.models_update(action),
            Message::Voice(action)=>return self.voice_update(action),
            Message::ChoosePortal(_) | Message::BaseUrl(_) | Message::Model(_) | Message::ApiKey(_) => {},
            Message::SaveProvider => {
                return self.models_update(models::Action::ApplyProvider(true));
            }
            Message::PortalStart if !self.running && !self.portal_starting && self.portal_cancel.is_none() => {
                self.portal_starting = true; self.banner = None; self.settings_view = settings::View::Accounts;
                self.portal_id = self.portal_id.wrapping_add(1); let id = self.portal_id;
                return Task::perform(async { portal::begin_login().await.map_err(|e| format!("{e:#}")) }, move |result| Message::PortalBegun(id, result));
            }
            Message::PortalStart => {},
            Message::PortalBegun(id, result) if id == self.portal_id => {
                self.portal_starting = false;
                match result {
                    Ok(login) => {
                        if let Err(error) = open_url(&login.verification_url) { self.error(error); }
                        let cancel = CancellationToken::new(); self.portal_cancel = Some(cancel.clone()); self.portal_login = Some(login.clone());
                        return Task::perform(async move { login.finish(cancel).await.map_err(|e| format!("{e:#}")) }, move |result| Message::PortalFinished(id, result));
                    }
                    Err(error) => self.error(format!("Could not start Nous Portal sign in: {error}")),
                }
            }
            Message::PortalFinished(id, result) if id == self.portal_id => {
                self.portal_connected=portal::is_available();
                self.portal_cancel = None; self.portal_login = None;
                match result {
                    Ok(()) => {
                        self.config = self.portal_config();
                        self.models.invalidate();self.models.load_options(&self.config);
                        if let Err(error) = Preferences::save(&self.config) { self.error(error); }
                        else {
                            self.note("Signed in with Nous Portal.");
                            if std::mem::take(&mut self.return_to_composer){
                                self.screen=Screen::Chat;
                                return Task::batch([self.discover_models(),widget::operation::focus(COMPOSER_ID)]);
                            }
                            self.screen=Screen::Provider;self.settings_view=settings::View::Accounts;return self.discover_models();
                        }
                    }
                    Err(error) => self.error(format!("Nous Portal sign in: {error}")),
                }
            }
            Message::PortalCancel => {
                if let Some(cancel) = self.portal_cancel.take() { cancel.cancel(); }
                self.portal_id = self.portal_id.wrapping_add(1); self.portal_starting = false;
                self.portal_login = None; self.note("Sign in cancelled.");
            }
            Message::PortalBegun(..) | Message::PortalFinished(..) => {},
            Message::WorkspacePath(value) => self.workspace_path = value,
            Message::AddWorkspace if !self.running => {
                let path = expand_path(self.workspace_path.trim());
                if let Some(library) = &self.library {
                    match library.add_project(&path) {
                        Ok(project) => {
                            let root = library.root().to_owned();
                            self.projects = library.projects().unwrap_or_default();
                            self.workspace_path.clear();
                            return Task::batch([
                                self.update(Message::SelectWorkspace(Some(project.id))),
                                self.sync_workspace_memory(root),
                            ]);
                        }
                        Err(error) => self.error(format!("Could not add workspace: {error:#}")),
                    }
                }
            }
            Message::SelectWorkspace(id) if !self.running => {
                if let Some(library) = &self.library {
                    match library.select_project(id.as_deref()) {
                        Ok(()) => {
                            self.project = library.selected_project().unwrap_or_default(); self.refresh_sessions();
                            if let Some(summary) = self.sessions.first().cloned() { self.open_chat(summary); } else { self.create_chat(); }
                            self.banner = None;
                        }
                        Err(error) => self.error(format!("Could not select workspace: {error:#}")),
                    }
                }
            }
            Message::AddWorkspace | Message::SelectWorkspace(_) => {},
            Message::RenameWorkspace(id) => {
                self.workspace_rename = self.projects.iter().find(|project| project.id == id)
                    .map(|project| (id, project.name.clone()));
            }
            Message::WorkspaceName(name) => {
                if let Some((_, current)) = &mut self.workspace_rename { *current = name; }
            }
            Message::CancelWorkspaceRename => self.workspace_rename = None,
            Message::SaveWorkspaceName => {
                if let (Some(library), Some((id, name))) = (&self.library, &self.workspace_rename) {
                    match library.rename_project(id, name) {
                        Ok(_) => {
                            let root = library.root().to_owned();
                            self.projects = library.projects().unwrap_or_default();
                            self.project = library.selected_project().unwrap_or_default();
                            self.workspace_rename = None;
                            self.banner = None;
                            return self.sync_workspace_memory(root);
                        }
                        Err(error) => self.error(format!("Could not rename project: {error:#}")),
                    }
                }
            }
            Message::WorkspaceMemorySynced(Ok(value)) if value["projects_ready"] == false => {
                self.note("Project saved. Its memory file will update after the current memory query.");
            }
            Message::WorkspaceMemorySynced(Err(error)) => {
                self.error(format!("Project saved. Could not update project memory: {error}"));
            }
            Message::WorkspaceMemorySynced(Ok(_)) => {}
            Message::ArchiveResults(scope, result) => {
                if self.archive_scope().as_ref() == Some(&scope) {
                    match result {
                        Ok(data) => {
                            let hits: Vec<_> = data["hits"].as_array().into_iter().flatten()
                                .filter_map(|hit| Some((hit["rel"].as_str()?.to_owned(), hit["text"].as_str()?.to_owned())))
                                .collect();
                            if hits.is_empty() {
                                self.archive_results = None;
                                self.note("No Archive results.");
                            } else {
                                self.archive_results = Some((scope, hits));
                            }
                        }
                        Err(error) => self.error(format!("Archive search failed: {error}")),
                    }
                }
            }
            Message::DismissArchiveResults => self.archive_results = None,
            Message::BrowseWorkspace if !self.running && !self.browsing => {
                self.browsing = true;
                return Task::perform(async { tokio::task::spawn_blocking(choose_folder).await.map_err(|e| e.to_string()).and_then(|r| r) }, Message::FolderChosen);
            }
            Message::BrowseWorkspace => {},
            Message::FolderChosen(result) => {
                self.browsing = false;
                let open_project = std::mem::take(&mut self.palette_open_folder);
                if open_project && let Ok(Some(path)) = &result {
                    self.workspace_path = path.to_string_lossy().into_owned();
                    return self.update(Message::AddWorkspace);
                }
                match result { Ok(Some(path)) => self.workspace_path = path.to_string_lossy().into_owned(), Ok(None) => {}, Err(error) => self.error(error) }
            }
            Message::Dismiss => self.banner = None,
            Message::RenameStart => self.rename = self.current.as_ref().map(|s| s.title.clone()),
            Message::RenameChanged(value) => self.rename = Some(value),
            Message::RenameSave => {
                if let (Some(current), Some(title)) = (&self.current, self.rename.clone()) {
                    let title = title.trim().to_owned();
                    if title.is_empty() || title.chars().count() > 200 {
                        self.error("Chat titles must contain 1-200 characters.");
                    } else if current.source == SessionSource::Native {
                        match self.rename_gateway(title, self.run_id) {
                            Ok(task) => return task,
                            Err(error) => self.error(error),
                        }
                    } else if let Some(library) = &self.library {
                        match library.rename_session(&current.id, &title) {
                            Ok(updated) => { self.current = Some(updated); self.rename = None; self.refresh_sessions(); }
                            Err(error) => self.error(format!("Could not rename chat: {error:#}")),
                        }
                    }
                }
            }
            Message::Copy(value) => return iced::clipboard::write(value),
            Message::Link(url) => {
                if url.starts_with("https://") || url.starts_with("http://") { if let Err(error) = open_url(&url) { self.error(error); } }
                else { self.note("Only HTTP and HTTPS links can be opened from a response."); }
            }
            Message::Scrolled(bottom) => self.follow_stream = bottom,
            Message::JumpToLatest => { self.follow_stream = true; return widget::operation::snap_to_end(SCROLL_ID); }
            Message::Shortcut(event) => return self.dispatch_keyboard(event, iced::event::Status::Ignored),
            Message::Files(files::Action::OpenFolder) => { self.screen = Screen::Workspaces; return self.update(Message::BrowseWorkspace); }
            Message::Files(action) => return self.files.update(action).map(Message::Files),
            Message::Terminal(terminal::Action::AddToChat(selection)) => {
                self.composer.perform(text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(selection))));
                if let Err(error) = self.save_draft() { self.error(error); }
                return widget::operation::focus(COMPOSER_ID);
            }
            Message::OpenAgentTerminal(terminal_id) => {
                if !hermes_engine::herdr::valid_id(&terminal_id) {
                    self.error("This result does not contain a valid agent terminal ID.");
                    return Task::none();
                }
                self.sync_terminal_context();
                self.screen = Screen::Terminal;
                self.right_pane = Some(RightPane::Terminal);
                return self.terminal.update(terminal::Action::AttachExisting(terminal_id)).map(Message::Terminal);
            }
            Message::Terminal(action) => return self.terminal.update(action).map(Message::Terminal),
            Message::Artifacts(action) => {
                let (task, effect) = self.artifacts.update(action, self.library.clone(), self.current.clone().map(|summary| (summary, self.history.clone())));
                #[cfg(feature = "ui-test")]
                if std::env::var_os("HERMES_RUST_OPEN_ARTIFACT").is_some() && self.artifacts.html_preview().is_some() { self.screen = Screen::Artifacts; }
                let effect = match effect {
                    Some(artifacts::Effect::OpenSession(id)) => {
                        if let Some(library) = &self.library {
                            match library.session(&id) {
                                Ok(summary) if !self.running => { self.open_chat(summary); Task::none() },
                                Ok(_) => { self.note("Stop the current response before opening another session."); Task::none() },
                                Err(error) => { self.error(format!("Could not open artifact session: {error:#}")); Task::none() },
                            }
                        } else { Task::none() }
                    }
                    Some(artifacts::Effect::OpenLink(url)) => { self.webview.clear_offer(); self.update(Message::Link(url)) }
                    Some(artifacts::Effect::OpenFile(path)) => {
                        let opener = if cfg!(target_os = "macos") { "open" } else if cfg!(windows) { "explorer" } else { "xdg-open" };
                        if let Err(error) = std::process::Command::new(opener).arg(&path).spawn() { self.error(format!("Could not open the file: {error}")); }
                        Task::none()
                    }
                    Some(artifacts::Effect::Copy(value)) => iced::clipboard::write(value), None => Task::none(),
                };
                return Task::batch([task.map(Message::Artifacts), effect]);
            }
            Message::Git(git::GitMessage::Close) => self.right_pane = None,
            Message::Scheduled(scheduled::Action::Close) => self.screen = Screen::Chat,
            Message::OpenRoutineChat(job) => {
                let Some(root) = self.library.as_ref().map(|library| library.root().to_owned()) else { return Task::none(); };
                let connection = remote::Connection::new(root, remote::Route::Local, self.config.clone());
                return Task::perform(async move {
                    let reply = connection.request("cron.chat", serde_json::json!({"job_id":job})).await?;
                    reply["session_id"].as_str().map(str::to_owned).ok_or_else(|| "Routine chat unavailable".to_owned())
                }, |result| Message::Scheduled(scheduled::Action::OpenedRoutine(result)));
            }
            Message::Cron(action) => {
                let ui = &mut self.cron_ui;
                let scheduled = match action {
                    CronUi::Toggle => { ui.open = !ui.open; None }
                    CronUi::More => { ui.visible += 10; None }
                    CronUi::Hover(id) => { ui.hover = id; None }
                    CronUi::Peek(id) => { ui.menu = None; ui.peek = if ui.peek.as_ref() == Some(&id) { None } else { Some(id) }; None }
                    CronUi::Menu(id) => { ui.menu = if ui.menu.as_ref() == Some(&id) { None } else { Some(id) }; None }
                    CronUi::Trigger(id) => { ui.menu = None; Some(scheduled::Action::Trigger(id)) }
                    CronUi::Pause(id, pause) => { ui.menu = None; Some(if pause { scheduled::Action::Pause(id) } else { scheduled::Action::Resume(id) }) }
                    CronUi::Manage(id) => { ui.menu = None; self.screen = Screen::Cron; Some(scheduled::Action::Select(id)) }
                    CronUi::Delete(id) => { ui.menu = None; self.screen = Screen::Cron; Some(scheduled::Action::Delete(id)) }
                };
                if let Some(action) = scheduled { return self.update(Message::Scheduled(action)); }
            }
            Message::Scheduled(scheduled::Action::Trigger(id)) => {
                let Some(root) = self.library.as_ref().map(|library| library.root().to_owned()) else { return Task::none(); };
                let connection = remote::Connection::new(root, remote::Route::Local, self.config.clone());
                let request_id = match self.scheduled.trigger_identity(&id) { Ok(id) => id, Err(error) => { self.error(error); return Task::none(); } };
                let task_job = id.clone(); let task_request = request_id.clone();
                return Task::perform(async move {
                    let reply = connection.request("cron.trigger", serde_json::json!({"job_id":id,"request_id":request_id})).await?;
                    reply["session_id"].as_str().map(str::to_owned).ok_or_else(|| reply["error"].as_str().unwrap_or("Scheduled run could not start").to_owned())
                }, move |result| Message::Scheduled(scheduled::Action::Triggered(task_job.clone(), task_request.clone(), result)));
            }
            Message::Scheduled(scheduled::Action::Triggered(job, request, result)) => {
                if result.is_ok() && let Err(error) = self.scheduled.clear_trigger_identity(&job, &request) { self.error(error); }
                return self.update(Message::Scheduled(scheduled::Action::OpenedRoutine(result)));
            }
            Message::Scheduled(scheduled::Action::OpenedRoutine(result)) => {
                match result {
                    Ok(id) if !self.running => if let Some(library) = &self.library && let Ok(summary) = library.session(&id) { self.open_chat(summary); self.screen = Screen::Chat; },
                    Ok(_) => self.note("Routine started. Open its run chat to review any approval requests."),
                    Err(error) => self.error(error),
                }
                return self.scheduled.update(scheduled::Action::Refresh, self.config.clone()).map(Message::Scheduled);
            }
            Message::Scheduled(scheduled::Action::OpenRunChat(id)) => {
                if self.running { self.note("Stop the current response before opening another session."); }
                else if let Some(library) = &self.library && let Ok(summary) = library.session(&id) { self.open_chat(summary); self.screen = Screen::Chat; }
            }
            Message::Scheduled(action) => {
                let task = self.scheduled.update(action, self.config.clone()).map(Message::Scheduled);
                if self.closing && !self.scheduled.has_running_jobs() { return task.chain(iced::exit()); }
                return task;
            }
            Message::LocalGateway(action) => {
                let profile = self.library.as_ref().map(|library| library.root().to_path_buf());
                let task = self.local_gateway.update(action, profile, self.config.clone()).map(Message::LocalGateway);
                if self.closing && !self.scheduled.has_running_jobs() { return task.chain(iced::exit()); }
                return task;
            }
            Message::Git(action) => return self.git.update(action).map(Message::Git),
            Message::Import(action) => {
                let (task, selected) = self.imports.update(action, self.library.clone(), self.project.as_ref().map(|p| p.id.clone()));
                self.refresh_sessions();
                if let Some(selected) = selected && !self.running { self.open_chat(selected); }
                return task.map(Message::Import);
            }
            Message::ProjectDiscovery(action) => {
                let (task, selected) = self.project_discovery.update(action, self.projects.clone());
                let mut tasks = vec![task.map(Message::ProjectDiscovery)];
                if let Some(path) = selected && !self.running {
                    self.workspace_path = path.to_string_lossy().into_owned();
                    tasks.push(self.update(Message::AddWorkspace));
                }
                return Task::batch(tasks);
            }
            Message::Backup(action) => {
                let running = self.running || self.scheduled.has_running_jobs();
                if matches!(&action, backup::Action::Export | backup::Action::Import) && !running {
                    if let Err(error) = self.save_draft().and_then(|_|self.save_queue()) { self.error(format!("Could not save current chat before backup: {error}")); return Task::none(); }
                }
                let imported = matches!(&action, backup::Action::Imported(Ok(_)));
                let task = self.backup.update(action, self.library.clone(), running).map(Message::Backup);
                if imported { self.refresh_sessions(); }
                return task;
            }
            Message::Messaging(action) => {
                let (task, effects) = self.messaging.update(action, self.library.clone(), self.library.as_ref().map(|lib| (lib.root().to_owned(), self.config.clone())));
                let mut tasks = vec![task.map(Message::Messaging)];
                for effect in effects {
                    match effect {
                        // Incoming messages are committed to the native inbox before
                        // delivery. Reading them never replaces the foreground chat.
                        messaging::Effect::Incoming(message) => { let _ = message.message_id; }
                        messaging::Effect::OpenLink(url) => tasks.push(self.update(Message::Link(url))),
                    }
                }
                return Task::batch(tasks);
            }
            Message::Close => {
                self.voice.stop_all();
                #[cfg(target_os="macos")]
                if let Some(library)=&self.library {if let Err(error)=window_bounds::save(library.root()){self.note(format!("Could not save window position: {error}"));}}
                if let Some(library) = &self.library && let Err(error) = self.appearance.flush(library.root()) { self.error(format!("Could not save appearance settings: {error}")); return Task::none(); }
                if let Err(error) = self.save_draft() { self.error(format!("Could not save your draft: {error}")); return Task::none(); }
                if let Some(detach) = self.chat.detach.take() { detach.cancel(); }
                self.messaging.shutdown();
                self.project_discovery.stop();
                if let Some(cancel) = self.portal_cancel.take() { cancel.cancel(); }
                if self.scheduled.has_running_jobs() { self.closing = true; self.scheduled.stop_all(); }
                if self.scheduled.has_running_jobs() { return Task::none(); }
                return iced::exit();
            }
            Message::SystemTheme(mode) => {
                self.system_dark = mode == iced::theme::Mode::Dark;
                #[cfg(feature = "ui-test")]
                if std::env::var_os("HERMES_RUST_TEST_CAPTURE").is_some() {
                    if let Ok(preference) = std::env::var("HERMES_RUST_TEST_THEME") {
                        match preference.as_str() { "light" => { self.dark = false; return Task::none(); }, "dark" => { self.dark = true; return Task::none(); }, _ => {} }
                    }
                }
                self.dark = self.appearance.preferences.dark(self.system_dark);
            }
            Message::ToggleSidebar => self.sidebar_open = !self.sidebar_open,
            Message::FlipPanes => self.flipped = !self.flipped,
            Message::ToggleLayout => self.layout_menu = !self.layout_menu,
            Message::ToggleAttach => self.attach_menu = !self.attach_menu,
            Message::Reasoning(action)=>return reasoning::update(self,action),
            Message::Notification(action)=>return notifications::update(self,action),
            Message::Billing(action)=>return billing::update(self,action),
            Message::Capability(action)=>return capabilities::update(self,action),
            Message::Bot(action)=>return bots::update(self,action),
            Message::Connection(action) => {
                let switch = matches!(&action, connections::Action::Save | connections::Action::UseSaved(_)
                    | connections::Action::GatewaySignedIn(_, Ok(())) | connections::Action::GatewaySignOut
                    | connections::Action::SshConnected(_, Ok(_)) | connections::Action::DisconnectSsh);
                let connection = connections::update(self, action);
                if switch { self.refresh_gateway_route(); }
                return connection;
            }
            Message::Onboarding(action)=>onboarding::update(self,action),
            Message::Slash(action)=>return slash::update(self,action),
            Message::Account(action) => return accounts::update(self, action),
            Message::Webhook(action) => return webhooks::update(self, action),
            Message::SubagentOpen(id) => self.subagent_open = id,
            Message::QuickEntry(action) => return quick_entry::update(self, action),
            Message::Overlay(action) => return overlays::update(self, action),
            Message::Pet(action) => return pets::update(self, action),
            Message::Profile(action) => return profiles::update(self, action),
            Message::Starmap(action) => return starmap::update(self, action),
            Message::OpenInWindow(id) => {
                if self.draft_files.pending > 0 {
                    self.note("Wait for the attachment to finish before opening another window.");
                    return Task::none();
                }
                if let Err(error) = self.save_draft() {
                    self.error(format!("Could not save your draft: {error}"));
                    return Task::none();
                }
                self.session_menu = None;
                match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("--open-session").arg(&id).spawn()) {
                    Ok(_) => self.note("Opened in a new window"),
                    Err(e) => self.error(format!("Could not open a new window: {e}")),
                }
            }
            Message::SyncTick => sync::tick(self),
            #[cfg(feature = "ui-test")]
            Message::HudTestInput => {
                #[cfg(target_os = "macos")]
                if let Some(id) = self.hud.window {
                    let probe = Task::perform(async { tokio::time::sleep(std::time::Duration::from_millis(1500)).await; }, |_| Message::HudTestInput);
                    if self.composer.text().trim().is_empty() { return Task::batch([iced::window::run(id, |w| hud_native::synthesize(w, 300.0, 52.0, "hi")).discard(), probe]); }
                    eprintln!("HUDINPUT composer={:?}", self.composer.text());
                }
            }
            Message::Hud(action) => { let window = self.webview.window; if matches!(action, hud::Action::Toggle) && !self.hud.on { self.screen = Screen::Chat; } let busy = self.running || !self.approvals.is_empty(); return self.hud.update(window, busy, action, Message::Hud); }
            // Only the app window hosts the preview webview; the HUD band never does.
            Message::WindowId(id) => {
                // The first window: come forward like any launched app (Electron does), so the window
                // opens on the Space the user is looking at instead of behind a full-screen app.
                if self.webview.window.is_none() && let Some(main) = id {
                    self.webview.window = id;
                    quick_entry::activate_app();
                    return Task::batch([iced::window::gain_focus(main), self.sync_webview()]);
                }
                return self.sync_webview();
            }
            Message::WindowClose(id) if self.overlays.pet == Some(id) => return self.update(Message::Overlay(overlays::Action::TogglePet)),
            Message::WindowClose(id) if self.overlays.wake == Some(id) => return Task::none(),
            Message::WindowClose(id) if self.quick_entry.window == Some(id) => return self.update(Message::QuickEntry(quick_entry::Action::Dismiss)),
            Message::WindowClose(id) if self.hud.window == Some(id) => return self.update(Message::Hud(hud::Action::Toggle)),
            Message::WindowClose(_) => return self.update(Message::Close),
            Message::WebviewTick => {}
            Message::WebviewBounds(bounds) => if self.webview.measured(bounds) { return self.sync_webview(); },
            Message::PortalAccent(accent) => if let Some(library) = &self.library && let Err(e) = shell_art::choose(library.root(), Some(accent), None) { self.error(format!("Could not save the color: {e}")); },
            Message::PortalArtwork(artwork) => {
                if let Some(library) = &self.library && let Err(e) = shell_art::choose(library.root(), None, Some(artwork)) { self.error(format!("Could not save the artwork: {e}")); }
                self.artwork = Some(artwork);
            }
            Message::ProfileSetting(action)=>return profile_settings::update(self,action),
            Message::Checkpoint(action)=>return checkpoint_ui::update(self,action),
            Message::SettingsView(view) => {
                self.shortcuts.cancel_capture();
                if view==settings::View::Vault { self.profile_settings.feedback=None; self.settings_view=view; self.screen=Screen::Provider; return vault::update(self,vault::Message::Refresh); }
                if view==settings::View::Sessions && let Some(lib)=&self.library {self.profile_settings.archived(lib);}
                self.profile_settings.feedback=None; self.settings_view = view; self.screen = Screen::Provider;
                if view==settings::View::Safety{return checkpoint_ui::update(self,checkpoint_ui::Action::Refresh);}
                if view==settings::View::Billing{return billing::update(self,billing::Action::Refresh);}
                if view==settings::View::Memory{return profile_settings::load_memory_providers(self);}
                if view==settings::View::Models && self.models.catalog.is_empty() && !self.models.loading && self.ready() {return self.update(Message::Models(models::Action::Refresh));} }
            Message::SettingsSearch(query) => self.settings_query = query,
            Message::SettingsApply => {
                if let Err(error) = self.config.validate() { self.error(error.to_string()); }
                else if let Err(error) = Preferences::save(&self.config) { self.error(format!("Could not save settings: {error}")); }
                else { self.note("Saved"); }
            }
            Message::Statusbar(statusbar::Action::Memory(None)) if self.statusbar.shows("system-resources") => {
                return Task::perform(async { tokio::task::spawn_blocking(statusbar::sample_memory).await.ok().flatten() },
                    |sample| Message::Statusbar(sample.map_or(statusbar::Action::Tick, |s| statusbar::Action::Memory(Some(s)))));
            }
            Message::Statusbar(statusbar::Action::SetApproval(mode)) => {
                let _ = self.statusbar.update(statusbar::Action::SetApproval(mode), None);
                if let Some(library) = &self.library {
                    let mut changes = serde_json::Map::new(); changes.insert("approval_mode".into(), serde_json::json!(mode));
                    match hermes_engine::settings::ProfileSettings::update(library.root(), &changes) {
                        Ok(saved) => self.profile_settings.saved = saved,
                        Err(error) => self.error(format!("Could not change the approval mode: {error:#}")),
                    }
                }
            }
            Message::Statusbar(action) => {
                if let Err(error) = self.statusbar.update(action, self.library.as_ref().map(|library| library.root())) { self.error(format!("Could not save status bar layout: {error}")); }
            }
            Message::Appearance(action) => {
                let Some(library) = &self.library else { self.error("Appearance settings need an open local library."); return Task::none(); };
                let result = self.appearance.update(action, library.root());
                self.dark = self.appearance.preferences.dark(self.system_dark);
                return match result { Ok(task) => task.map(Message::Appearance), Err(error) => { self.error(format!("Could not save appearance settings: {error}")); Task::none() } };
            }
            Message::QueueCurrent => {
                if !self.running { return self.send(); }
                if self.group_round.is_none() { return self.submit_gateway(self.composer.text().trim().to_owned()); }
                self.queue_from_composer(false);
            }
            Message::QueueRemove(id) => {
                self.queue.entries.retain(|entry| entry.id != id);
                if self.queue_edit.as_ref().is_some_and(|edit| edit.0 == id) { self.queue_edit = None; }
                if let Err(error) = self.save_queue() { self.error(error); }
            }
            Message::QueueSendNow(id) => {
                if let Some(index) = self.queue.entries.iter().position(|entry| entry.id == id) {
                    let entry = self.queue.entries.remove(index); self.queue.entries.insert(0, entry); self.queue.parked = false;
                    if let Err(error) = self.save_queue() { self.queue.parked = true; self.error(error); return Task::none(); }
                    if self.running { self.redirecting = true; self.cancel_response(); } else { return Task::none(); }
                }
            }
            Message::QueueResume => {
                if self.group_round.is_none() {
                    let Some(current) = self.current.as_ref() else { return Task::none(); };
                    let current_id = current.id.clone();
                    let tasks: Vec<_> = self.chat.pending_queue.iter()
                        .filter(|entry| entry.status == "parked")
                        .map(|entry| self.change_pending(current_id.clone(), entry.id.clone(), chat_view::QueueAction::Resume))
                        .collect();
                    if tasks.is_empty() {
                        self.note("No parked gateway messages to resume.");
                        return Task::none();
                    }
                    return Task::batch(tasks);
                }
                self.queue.parked = false;
                if let Err(error) = self.save_queue() { self.queue.parked = true; self.error(error); }
                else { return Task::none(); }
            }
            Message::QueuePause => {
                if self.group_round.is_none() {
                    if self.running {
                        self.note("Paused. Gateway queued messages will wait until resumed.");
                        return self.update(Message::Stop);
                    }
                    if self.chat.pending_queue.iter().any(|entry| entry.status == "parked") {
                        self.note("Gateway queue is already paused.");
                    } else {
                        self.note("No active gateway turn to pause.");
                    }
                } else {
                    self.queue.parked = true;
                    if let Err(error) = self.save_queue() { self.error(error); }
                    self.note("Paused. Queued messages wait until you resume.");
                    if self.running { return self.update(Message::Stop); }
                }
            }
            Message::QueueEdit(id) => self.queue_edit = self.queue.entries.iter().find(|entry| entry.id == id).map(|entry| (id, entry.text.clone())),
            Message::QueueEditChanged(text) => { if let Some(edit) = &mut self.queue_edit { edit.1 = text; } }
            Message::QueueEditCancel => self.queue_edit = None,
            Message::QueueEditSave => {
                if let Some((id, text)) = &self.queue_edit {
                    let text = text.trim();
                    if text.is_empty() || text.len() > 128 * 1024 { self.error("Enter a queued message of at most 128 KiB."); return Task::none(); }
                    if let Some(entry) = self.queue.entries.iter_mut().find(|entry| entry.id == *id) { entry.text = text.into(); }
                    if let Err(error) = self.save_queue() { self.error(error); } else { self.queue_edit = None; }
                }
            }
            Message::DragWindow => return iced::window::oldest().and_then(iced::window::drag),
            #[cfg(feature = "ui-test")]
            Message::Capture => return match self.overlays.pet.or(self.quick_entry.window).or(self.hud.window) { Some(hud) => iced::window::screenshot(hud).map(Message::Captured), None => iced::window::oldest().and_then(iced::window::screenshot).map(Message::Captured) },
            #[cfg(feature = "ui-test")]
            Message::Captured(screenshot) => {
                if let Some(path) = std::env::var_os("HERMES_RUST_TEST_CAPTURE") {
                    let path = PathBuf::from(path);
                    if path.is_absolute() {
                        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
                        if let Err(error) = image::save_buffer_with_format(&path, &screenshot.rgba, screenshot.size.width, screenshot.size.height, image::ColorType::Rgba8, image::ImageFormat::Png) { eprintln!("Native UI capture failed: {error}"); }
                    }
                }
                return self.update(Message::Close);
            }
        }
        Task::none()
    }
}

fn palette_keyboard_event(event: iced::Event, status: iced::event::Status, _: iced::window::Id) -> Option<Message> {
    let iced::Event::Keyboard(event) = event else { return None; };
    Some(Message::GlobalKey(event, status))
}

fn draft_surface(summary: &SessionSummary, history: &History) -> bool {
    history.turns.is_empty() && summary.title == "New chat"
        && matches!(summary.source, hermes_library::SessionSource::Native | hermes_library::SessionSource::LegacyRust)
}

fn expand_path(value: &str) -> PathBuf {
    if (value == "~" || value.starts_with("~/")) && let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(value.trim_start_matches('~').trim_start_matches('/'));
    }
    PathBuf::from(value)
}
fn choose_folder()->Result<Option<PathBuf>,String>{
    picker::choose("Choose a Hermes workspace",true)
}

fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = { let mut cmd = std::process::Command::new("rundll32"); cmd.arg("url.dll,FileProtocolHandler"); cmd };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url).spawn().map(|_| ()).map_err(|e| format!("Could not open link: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_messages_are_redacted() {
        let secret = "provider-secret-that-must-not-appear";
        let debug = format!("{:?}", Message::ApiKey(secret.into()));
        assert!(!debug.contains(secret)); assert!(debug.contains("redacted"));
    }
    #[test]
    fn preferences_have_no_credential_field() {
        let value = serde_json::to_value(Preferences { base_url: "https://example.test/v1".into(), model: "example".into() }).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2); assert!(value.get("api_key").is_none());
    }
}
