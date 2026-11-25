//! Messaging platform catalog (hermes_cli/web_server_messaging.py + every
//! plugins/platforms/*/plugin.yaml): name, description, required and optional
//! fields. Settings are kept per library; secret fields go to a 0600 file and
//! are never shown again. Telegram has its own live native adapter
//! (messaging.rs); the other adapters run in the Hermes backend.
use super::{credential_files, messaging::Message};
use crate::appearance::text;
use iced::{Alignment, Border, Color, Element, Fill, Padding, widget::{Space, button, column, container, row, rule, scrollable, text_input}};
use hermes_engine::secret;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{collections::{BTreeMap, BTreeSet}, path::{Path, PathBuf}, sync::LazyLock};

#[derive(Clone, Debug, Deserialize)]
pub(super) struct Field { pub name: String, pub label: String, #[serde(default)] pub description: String, #[serde(default)] pub secret: bool, #[serde(default)] pub url: String, #[serde(default)] pub required: bool }
#[derive(Clone, Debug, Deserialize)]
pub(super) struct Platform { pub id: String, pub name: String, #[serde(default)] pub description: String, #[serde(default)] pub fields: Vec<Field> }

pub(super) static CATALOG: LazyLock<Vec<Platform>> = LazyLock::new(|| {
    let mut all: Vec<Platform> = serde_json::from_str(include_str!("../assets/messaging-platforms.json")).unwrap_or_default();
    all.retain(|p| p.id != "telegram"); // Telegram is the native live adapter.
    all
});

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved { enabled: BTreeSet<String>, values: BTreeMap<String, BTreeMap<String, String>> }

#[derive(Clone, Debug)]
pub(super) enum Action { Select(String), Field(String, String), Save, Toggle }

#[derive(Default)]
pub(super) struct State {
    pub selected: Option<String>, saved: Saved, secrets: BTreeMap<String, BTreeSet<String>>,
    draft: BTreeMap<String, String>, dirty: bool, note: Option<Result<String, String>>, root: Option<PathBuf>,
}

fn values_path(root: &Path) -> PathBuf { root.join("messaging-platforms.json") }
fn secrets_path(root: &Path) -> PathBuf { root.join("messaging-secrets.json") }

type Secrets = BTreeMap<String, BTreeMap<String, String>>;

fn read_json<T: Default + DeserializeOwned>(path: &Path) -> Result<T, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| format!(
            "{} is damaged, so nothing was saved. Move it out of the Hermes data folder and enter these settings again.",
            path.file_name().unwrap_or_default().to_string_lossy()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_json(path: &Path, data: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(data).map_err(|error| error.to_string())?;
    credential_files::replace(path, &bytes)
}

/// Protect legacy plaintext values while preserving ciphertext even when its key is unavailable.
fn seal_secrets(root: &Path, secrets: &mut Secrets) -> Result<bool, String> {
    let mut changed = false;
    for value in secrets.values_mut().flat_map(BTreeMap::values_mut).filter(|value| !secret::is_sealed(value)) {
        let sealed = secret::seal(root, value).map_err(|error| format!("{error:#}"))?;
        if secret::is_sealed(&sealed) {
            *value = sealed;
            changed = true;
        }
    }
    Ok(changed)
}

fn migrate_secrets(root: &Path) -> Result<Secrets, String> {
    let path = secrets_path(root);
    if read_json::<Secrets>(&path).is_ok_and(|saved| saved.values().flat_map(BTreeMap::values).any(|value| !secret::is_sealed(value))) {
        secret::prepare(root);
    }
    let _lock = credential_files::lock(&path)?;
    let mut secrets = read_json(&path)?;
    if seal_secrets(root, &mut secrets)? {
        write_json(&path, &secrets)?;
    }
    Ok(secrets)
}

impl State {
    pub fn load(&mut self, root: &Path) {
        self.root = Some(root.to_owned());
        self.saved = read_json(&values_path(root)).unwrap_or_default();
        let secrets = migrate_secrets(root).or_else(|_| read_json(&secrets_path(root))).unwrap_or_default();
        self.secrets = secrets.into_iter().map(|(p, m)| (p, m.into_keys().collect())).collect();
    }
    fn platform(&self) -> Option<&'static Platform> { let id = self.selected.as_deref()?; CATALOG.iter().find(|p| p.id == id) }
    /// Every required field has a saved value (or a saved secret).
    pub fn configured(&self, platform: &Platform) -> bool {
        platform.fields.iter().filter(|f| f.required).all(|f| self.has_value(&platform.id, f))
    }
    fn has_value(&self, platform: &str, field: &Field) -> bool {
        if field.secret { self.secrets.get(platform).is_some_and(|s| s.contains(&field.name)) }
        else { self.saved.values.get(platform).and_then(|m| m.get(&field.name)).is_some_and(|v| !v.trim().is_empty()) }
    }
    pub fn enabled(&self, id: &str) -> bool { self.saved.enabled.contains(id) }
    fn persist(&mut self, enabled: Option<bool>) -> Result<(), String> {
        let Some(root) = self.root.as_deref() else { return Err("The chat library is unavailable.".into()) };
        let Some(platform) = self.platform() else { return Ok(()) };
        let path = secrets_path(root);
        if platform.fields.iter().any(|field| field.secret && self.draft.get(&field.name).is_some_and(|value| !value.trim().is_empty())) {
            secret::prepare(root);
        }
        let _lock = credential_files::lock(&path)?;
        let mut secrets: Secrets = read_json(&path)?;
        let mut saved: Saved = read_json(&values_path(root))?;
        let mut added_secret = false;
        for field in &platform.fields {
            let Some(value) = self.draft.get(&field.name) else { continue };
            if field.secret {
                if !value.trim().is_empty() {
                    secrets.entry(platform.id.clone()).or_default().insert(field.name.clone(), value.trim().to_owned());
                    added_secret = true;
                }
            } else {
                saved.values.entry(platform.id.clone()).or_default().insert(field.name.clone(), value.trim().to_owned());
            }
        }
        if let Some(on) = enabled {
            if on { saved.enabled.insert(platform.id.clone()); } else { saved.enabled.remove(&platform.id); }
        }
        if seal_secrets(root, &mut secrets)? || added_secret {
            write_json(&path, &secrets)?;
        }
        write_json(&values_path(root), &saved)?;
        self.saved = saved;
        self.secrets = secrets.into_iter().map(|(p, m)| (p, m.into_keys().collect())).collect();
        self.draft.clear(); self.dirty = false;
        Ok(())
    }
    pub fn update(&mut self, action: Action) {
        match action {
            Action::Select(id) => { self.selected = Some(id); self.draft.clear(); self.dirty = false; self.note = None; }
            Action::Field(name, value) => { self.draft.insert(name, value); self.dirty = true; self.note = None; }
            Action::Save => self.note = Some(self.persist(None).map(|_| "Saved.".into())),
            Action::Toggle => {
                let Some(platform) = self.platform() else { return };
                if self.dirty && let Err(e) = self.persist(None) { self.note = Some(Err(e)); return; }
                if !self.configured(platform) { self.note = Some(Err("Fill in the required fields first.".into())); return; }
                let on = !self.saved.enabled.contains(&platform.id);
                self.note = Some(self.persist(Some(on)).map(|_| if on { "Enabled. The Hermes backend starts this channel on its next run.".into() } else { "Disabled.".into() }));
            }
        }
    }
}

fn status_label(state: &State, platform: &Platform) -> &'static str {
    if state.enabled(&platform.id) { "Enabled" } else if state.configured(platform) { "Ready" } else { "Needs setup" }
}

/// Rail rows for every catalog platform matching the search.
pub(super) fn rail_rows<'a>(state: &'a State, query: &str, p: super::ui::Palette) -> Vec<Element<'a, Message>> {
    let query = query.trim().to_lowercase();
    CATALOG.iter().filter(|pl| query.is_empty() || pl.name.to_lowercase().contains(&query) || pl.id.contains(&query)).map(|platform| {
        let selected = state.selected.as_deref() == Some(platform.id.as_str());
        let dot = if state.enabled(&platform.id) { p.accent } else { p.muted };
        let initial = platform.name.chars().next().unwrap_or('?').to_string();
        button(row![
            container(text(initial).size(11).font(super::ui::medium())).width(18).height(18).center_x(18).center_y(18)
                .style(move |_| super::ui::panel(Color { a: 0.12, ..p.accent }, 9.0, None)),
            text(platform.name.clone()).size(13).width(Fill), text("●").size(9).color(dot)].spacing(8).align_y(Alignment::Center))
            .on_press(Message::Platform(Action::Select(platform.id.clone()))).padding([7, 8]).width(Fill)
            .style(move |_, status| button::Style { background: (selected || status == button::Status::Hovered).then(|| p.card.into()), text_color: p.foreground,
                border: Border { radius: 6.0.into(), ..Default::default() }, ..Default::default() }).into()
    }).collect()
}

/// The setup form for the selected platform, as the original's platform card.
pub(super) fn detail<'a>(state: &'a State, dark: bool) -> Element<'a, Message> {
    let p = super::ui::palette(dark);
    let Some(platform) = state.platform() else { return Space::new().into() };
    let chip = |label: &'static str| container(text(label).size(11).color(p.muted)).padding([3, 7]).style(move |_| super::ui::panel(p.card, 6.0, None));
    let heading = |label: &'static str| text(label).size(11).font(super::ui::medium()).color(p.muted);
    let mut body = column![
        row![text(platform.name.clone()).size(15).font(super::ui::medium()), chip(status_label(state, platform))].spacing(10).align_y(Alignment::Center),
        text(platform.description.clone()).size(12).color(p.muted),
    ].spacing(14);
    if let Some(guide) = platform.fields.iter().find(|f| !f.url.is_empty()) {
        body = body.push(heading("GET YOUR CREDENTIALS")).push(button(row![text("Open setup guide").size(12), super::ui::icon("link-external", 11, p.muted)].spacing(6))
            .padding(0).style(button::text).on_press(Message::OpenLink(guide.url.clone())));
    }
    for (title, required) in [("REQUIRED", true), ("OPTIONAL", false)] {
        let fields: Vec<&Field> = platform.fields.iter().filter(|f| f.required == required).collect();
        if fields.is_empty() { continue; }
        body = body.push(heading(title));
        for field in fields {
            let saved = state.saved.values.get(&platform.id).and_then(|m| m.get(&field.name)).cloned().unwrap_or_default();
            let value = state.draft.get(&field.name).cloned().unwrap_or(if field.secret { String::new() } else { saved });
            let placeholder = if field.secret && state.has_value(&platform.id, field) { "Saved · paste to replace".to_owned() } else { field.label.clone() };
            let name = field.name.clone();
            let mut input = text_input(&placeholder, &value).size(13).padding([7, 10]).on_input(move |v| Message::Platform(Action::Field(name.clone(), v)));
            if field.secret { input = input.secure(true); }
            let mut block = column![text(field.label.clone()).size(13)].spacing(5);
            if !field.description.is_empty() && field.description != field.label { block = block.push(text(field.description.clone()).size(12).color(p.muted)); }
            body = body.push(block.push(input));
        }
    }
    body = body.push(text("This platform's adapter runs in the Hermes backend; settings saved here are what it reads. Telegram also has a live native adapter in this build.").size(12).color(p.muted));
    if let Some(note) = &state.note {
        body = body.push(text(match note { Ok(s) | Err(s) => s.as_str() }).size(12).color(if note.is_ok() { p.muted } else { Color::from_rgb8(232, 98, 98) }));
    }
    let enabled = state.enabled(&platform.id);
    let actions = row![text(status_label(state, platform)).size(12).color(p.muted), Space::new().width(Fill),
        button(text("Save changes").size(12)).padding([7, 12]).on_press_maybe(state.dirty.then_some(Message::Platform(Action::Save))),
        button(text(if enabled { "Disable" } else { "Enable" }).size(12)).padding([7, 12]).on_press(Message::Platform(Action::Toggle))].spacing(10).align_y(Alignment::Center);
    column![
        scrollable(container(body).padding(Padding { top: 16.0, right: 24.0, bottom: 24.0, left: 24.0 }).max_width(720).width(Fill)).height(Fill),
        rule::horizontal(1), container(actions).padding([12, 20]),
    ].height(Fill).width(Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_reads_secrets_after_a_concurrent_replacement() {
        use std::{sync::mpsc, time::Duration};

        let dir = tempfile::tempdir().unwrap();
        let path = secrets_path(dir.path());
        write_json(&path, &serde_json::json!({"discord": {"DISCORD_BOT_TOKEN": "legacy"}})).unwrap();
        let guard = credential_files::lock(&path).unwrap();

        let (started, starting) = mpsc::channel();
        let (finished, loaded) = mpsc::channel();
        let root = dir.path().to_owned();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            let mut state = State::default();
            state.load(&root);
            let _ = finished.send(state.secrets);
        });
        starting.recv().unwrap();
        assert_eq!(loaded.recv_timeout(Duration::from_millis(75)), Err(mpsc::RecvTimeoutError::Timeout));
        write_json(&path, &serde_json::json!({"slack": {"SLACK_BOT_TOKEN": "replacement"}})).unwrap();
        drop(guard);

        let secrets = loaded.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
        assert!(!secrets.contains_key("discord"));
        assert!(secrets["slack"].contains("SLACK_BOT_TOKEN"));
    }

    #[test]
    fn a_damaged_settings_file_is_named_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(values_path(dir.path()), b"{").unwrap();
        let mut state = State::default();
        state.load(dir.path());
        state.update(Action::Select("discord".into()));
        state.update(Action::Save);
        let error = state.note.clone().unwrap().unwrap_err();
        assert!(error.contains("messaging-platforms.json is damaged"), "{error}");
        assert_eq!(std::fs::read(values_path(dir.path())).unwrap(), b"{");
    }

    #[test]
    fn catalog_lists_the_hermes_platforms_and_saves_privately() {
        assert!(CATALOG.len() >= 25 && CATALOG.iter().any(|p| p.id == "discord") && CATALOG.iter().any(|p| p.id == "slack"));
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default(); state.load(dir.path());
        state.update(Action::Select("discord".into()));
        state.update(Action::Toggle);
        assert!(!state.enabled("discord"), "cannot enable without the required token");
        state.update(Action::Field("DISCORD_BOT_TOKEN".into(), "secret-token".into()));
        state.update(Action::Toggle);
        assert!(state.enabled("discord"));
        assert!(!std::fs::read_to_string(dir.path().join("messaging-platforms.json")).unwrap().contains("secret-token"));
        let mut reopened = State::default(); reopened.load(dir.path());
        assert!(reopened.enabled("discord") && reopened.configured(CATALOG.iter().find(|p| p.id == "discord").unwrap()));
    }
}
