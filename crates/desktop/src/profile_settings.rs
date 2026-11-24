//! Native settings for the current profile. Apply validates a merged settings
//! record and commits only the controls edited on this page.
use crate::appearance::text;
use super::{Hermes, Message, settings::View, ui};
use hermes_engine::settings::{MemoryProvider, PERSONALITIES, ProfileSettings};
#[cfg(feature = "reliquary")]
use hermes_reliquary_plugin::{EmbeddingNormalization, HostedEmbeddingSettings, HostedModelRoute, HostedSwitchboardConfig, ModelSlot};
use hermes_library::{ArchiveFilter,Library,SessionQuery,SessionSource,SessionSummary};
use iced::{Alignment, Element, Fill, Task, widget::{button,column,container,pick_list,row,text_input,Space}};
use std::{collections::BTreeMap,path::{Path,PathBuf},process::{Child,Command}};

#[derive(Clone,Copy,PartialEq,Eq)]
enum Kind { Text, Count, Check, Personality, Directory }
#[derive(Clone, Copy)]
struct Field { page:View, key:&'static str, label:&'static str, description:&'static str, kind:Kind }
const fn field(page:View,key:&'static str,label:&'static str,description:&'static str,kind:Kind)->Field{Field{page,key,label,description,kind}}
const FIELDS:&[Field]=&[
    field(View::Chat,"personality","Personality","Default assistant style for new sessions.",Kind::Personality),
    field(View::Chat,"timezone","Timezone","Timezone for the agent and local clock. Leave blank for the system default.",Kind::Text),
    field(View::Workspace,"default_directory","Default project folder","Working directory for new chats outside a selected project.",Kind::Directory),
    field(View::Workspace,"file_read_max_chars","File read character limit","Maximum characters returned by one file read.",Kind::Count),
    field(View::Memory,"memory_enabled","Agent notes","Keep agent notes in MEMORY.md.",Kind::Check),
    field(View::Memory,"memory_char_limit","Memory character limit","Maximum combined length of agent memory entries.",Kind::Count),
    field(View::Memory,"user_profile_enabled","User notes","Keep preference notes in USER.md.",Kind::Check),
    field(View::Memory,"user_char_limit","User profile character limit","Maximum combined length of user profile entries.",Kind::Count),
    field(View::Safety,"redact_secrets","Redact secrets","Mask detected credentials in text sent to the model.",Kind::Check),
    field(View::Safety,"approval_timeout","Approval timeout","Seconds to wait for a response before declining a tool request.",Kind::Count),
    field(View::Safety,"checkpoints_enabled","Filesystem checkpoints","Create rollback snapshots before approved native file writes.",Kind::Check),
    field(View::Safety,"checkpoint_max_snapshots","Checkpoint retention","Maximum retained file snapshots (1–100).",Kind::Count),
    field(View::Advanced,"clock_tools","System clock","Allow Hermes to request the local time tool.",Kind::Check),
    field(View::Advanced,"file_tools","File tools","Allow Hermes to request directory listing, search, read and write tools.",Kind::Check),
    field(View::Advanced,"skill_tools","Skills","Allow Hermes to discover and read workspace skills.",Kind::Check),
    field(View::Advanced,"memory_tool","Memory tools","Allow Hermes to call the selected provider’s memory tools.",Kind::Check),
    field(View::Advanced,"mcp_tools","MCP tools","Connect to the configured MCP server and offer its tools to Hermes.",Kind::Check),
    field(View::Advanced,"max_turns","Maximum iterations","Maximum tool-call rounds per agent turn.",Kind::Count),
    field(View::Advanced,"tool_output_bytes","Maximum output bytes","Size limit for each tool result.",Kind::Count),
    field(View::Advanced,"tool_output_lines","Maximum output lines","Line limit for tool output.",Kind::Count),
    field(View::Advanced,"tool_output_line_length","Maximum line length","Character limit for each line of tool output.",Kind::Count),
    #[cfg(target_os="macos")]
    field(View::Advanced,"keep_awake","Keep computer awake","Prevent idle system sleep while Hermes Desktop is open.",Kind::Check),
    field(View::Sessions,"auto_archive","Auto-archive inactive sessions","Automatically archive inactive sessions. Pinned chats stay in your session list.",Kind::Check),
    field(View::Sessions,"auto_archive_days","Days of inactivity","How long to wait before archiving an inactive chat.",Kind::Count),
];
#[derive(Clone,Debug)]
pub(super) enum Action {
    SetMemoryProvider(MemoryProvider),
    MemorySynced(Result<serde_json::Value, String>),
    #[cfg(feature = "reliquary")]
    SwitchboardEndpoint(ModelSlot, String),
    #[cfg(feature = "reliquary")]
    SwitchboardModel(ModelSlot, String),
    #[cfg(feature = "reliquary")]
    SwitchboardReasoning(ModelSlot, String),
    #[cfg(feature = "reliquary")]
    SwitchboardDimensions(String),
    #[cfg(feature = "reliquary")]
    SwitchboardNormalization(String),
    #[cfg(feature = "reliquary")]
    SaveSwitchboard,
    Set(&'static str,String,View), Edit(&'static str,String),Apply(View),Browse,DirectoryChosen(Result<Option<PathBuf>,String>),
    Restore(String),RestoreDone(String,Result<serde_json::Value,String>),Delete(String),DeleteDone(String,Result<serde_json::Value,String>),CancelDelete,ConfirmDelete,Maintain,MaintainDone(Result<serde_json::Value,String>),
}
#[derive(Default)]
pub(super) struct State {
    pub saved:ProfileSettings,
    #[cfg(feature = "reliquary")]
    switchboard: Option<HostedSwitchboardConfig>,
    #[cfg(feature = "reliquary")]
    switchboard_edits: Vec<(ModelSlot, Option<HostedModelRoute>)>,
    #[cfg(feature = "reliquary")]
    switchboard_dimensions: String,
    #[cfg(feature = "reliquary")]
    switchboard_normalization: String,
    #[cfg(feature = "reliquary")]
    switchboard_embedding_dirty: bool,
    edits:BTreeMap<&'static str,String>,
    pub archive:Vec<SessionSummary>,
    pub feedback:Option<Result<String,String>>,
    delete:Option<String>,
    awake:Option<Child>,
}
impl Drop for State{fn drop(&mut self){self.release_awake();}}
impl State {
    fn release_awake(&mut self){if let Some(mut child)=self.awake.take(){let _=child.kill();let _=child.wait();}}
    fn sync_awake(&mut self)->Result<(),String>{
        #[cfg(target_os="macos")]
        if self.saved.keep_awake && self.awake.is_none(){
            self.awake=Some(Command::new("/usr/bin/caffeinate").args(["-i","-w",&std::process::id().to_string()]).spawn().map_err(|e|e.to_string())?);return Ok(());
        }
        if !self.saved.keep_awake {self.release_awake();}
        Ok(())
    }
    pub fn load(root:&Path)->Self{
        let mut state=Self::default();
        match ProfileSettings::load(root) {
            Ok(saved)=>{
                state.saved=saved;
            }
            Err(e)=>state.feedback=Some(Err(e.to_string())),
        }
        #[cfg(feature = "reliquary")]
        {
            match hermes_reliquary_plugin::switchboard_config(root) {
                Ok(config) => state.switchboard = Some(config),
                Err(error) => state.feedback=Some(Err(format!("Could not load Reliquary Switchboard settings: {error:#}"))),
            }
            state.sync_switchboard_embedding();
        }
        if let Err(e)=state.sync_awake(){state.feedback=Some(Err(e));}
        state
    }
    #[cfg(feature = "reliquary")]
    fn sync_switchboard_embedding(&mut self) {
        let embedding = self.switchboard.as_ref()
            .and_then(|config| config.model(ModelSlot::Embedding))
            .and_then(|route| route.embedding.as_ref());
        self.switchboard_dimensions = embedding
            .map(|settings| settings.dimensions.to_string())
            .unwrap_or_default();
        self.switchboard_normalization = match embedding.map(|settings| settings.normalization) {
            Some(EmbeddingNormalization::L2) => "L2",
            _ => "None",
        }.into();
        self.switchboard_embedding_dirty = false;
    }

    pub fn archived(&mut self,lib:&Library){
        match lib.list_sessions(&SessionQuery{archive:ArchiveFilter::Archived,..Default::default()}){
            Ok(archive)=>self.archive=archive,Err(e)=>self.feedback=Some(Err(e.to_string())),
        }
    }
    fn value(&self,f:&Field)->String{
        if let Some(v)=self.edits.get(f.key){return v.clone();}
        let v=serde_json::to_value(&self.saved).unwrap_or_default();
        match &v[f.key] {serde_json::Value::Null=>String::new(),serde_json::Value::String(s)=>s.clone(),other=>other.to_string()}
    }
    fn apply(&mut self,page:View,root:&Path)->Result<(),String>{
        let mut changes=serde_json::Map::new();
        for f in FIELDS.iter().filter(|f|f.page==page){
            let Some(entry)=self.edits.get(f.key) else{continue;};
            let entry=entry.trim();
            changes.insert(f.key.to_string(),match f.kind {
                Kind::Count=>serde_json::json!(entry.parse::<u64>().map_err(|_|format!("Enter a whole number for {}.",f.label))?),
                Kind::Check=>serde_json::json!(entry.parse::<bool>().map_err(|_|"Invalid setting")?),
                Kind::Directory if entry.is_empty()=>serde_json::Value::Null,
                Kind::Directory=>{let path=super::expand_path(entry).canonicalize().map_err(|_|"Choose an existing project folder")?;
                    if !path.is_dir(){return Err("Choose a project folder".into())}serde_json::json!(path)}
                _=>serde_json::json!(entry),
            });
        }
        let next=ProfileSettings::update(root,&changes).map_err(|e|format!("{e:#}"))?;
        self.saved=next;self.edits.retain(|k,_|!FIELDS.iter().any(|f|f.key==*k && f.page==page));
        self.sync_awake()
    }
}
#[cfg(feature = "reliquary")]
fn switchboard_route(state: &State, slot: ModelSlot) -> Option<&HostedModelRoute> {
    let pending = state.switchboard_edits.iter().rev().find(|(edited,_)|*edited==slot)
        .map(|(_,route)|route.as_ref());
    effective_route(pending,state.switchboard.as_ref().and_then(|config|config.model(slot)))
}
#[cfg(any(feature = "reliquary", test))]
fn effective_route<'a, T>(pending: Option<Option<&'a T>>, saved: Option<&'a T>) -> Option<&'a T> {
    match pending { Some(route) => route, None => saved }
}
#[cfg(feature = "reliquary")]
fn route_for_edit(state: &State, slot: ModelSlot) -> Option<HostedModelRoute> {
    switchboard_route(state,slot).cloned()
}
#[cfg(feature = "reliquary")]
fn switchboard_edit(state: &mut State, slot: ModelSlot, route: Option<HostedModelRoute>) {
    if let Some((_,existing))=state.switchboard_edits.iter_mut().find(|(edited,_)|*edited==slot) {
        *existing=route;
    } else { state.switchboard_edits.push((slot,route)); }
    state.feedback=None;
}
#[cfg(feature = "reliquary")]
fn nonempty(value: String) -> Option<String> {
    let value=value.trim().to_owned(); (!value.is_empty()).then_some(value)
}
pub(super) fn update(app:&mut Hermes,action:Action)->Task<Message>{
    let Some(lib)=app.library.clone() else{return Task::none()};
    let state=&mut app.profile_settings;
    match action {
        Action::SetMemoryProvider(provider) => {
            let changes = serde_json::Map::from_iter([("memory_provider".into(), serde_json::json!(provider))]);
            state.feedback = Some(ProfileSettings::update(lib.root(), &changes)
                .map(|saved| { state.saved = saved; "Settings saved".into() })
                .map_err(|e| format!("{e:#}")));
            if provider == MemoryProvider::Reliquary
                && hermes_gateway::stock_plugins::has_memory_plugin()
                && state.feedback.as_ref().is_some_and(Result::is_ok)
            {
                let root = lib.root().to_owned();
                return Task::perform(
                    super::local_gateway::catalog_request(root, app.config.clone(), "plugins.memory.sync", serde_json::json!({})),
                    |result| Message::ProfileSetting(Action::MemorySynced(result)),
                );
            }
        }
        Action::MemorySynced(result) => {
            if state.saved.memory_provider == MemoryProvider::Reliquary {
                state.feedback = Some(result.and_then(|result| {
                    if result["enrolled"] != true {
                        return Err("This profile's gateway does not have the Reliquary stock plugin.".into());
                    }
                    Ok("Reliquary enrolled.".into())
                }));
            }
        }
        #[cfg(feature = "reliquary")]
        Action::SwitchboardEndpoint(slot, endpoint) => {
            let route = if endpoint.is_empty() {
                None
            } else {
                let (model, reasoning_effort, embedding) = route_for_edit(state, slot)
                    .map(|route| (route.model, route.reasoning_effort, route.embedding))
                    .unwrap_or_default();
                Some(HostedModelRoute { endpoint, model, reasoning_effort, embedding })
            };
            switchboard_edit(state, slot, route);
        }
        #[cfg(feature = "reliquary")]
        Action::SwitchboardModel(slot, model) => {
            if let Some(mut route) = route_for_edit(state, slot) {
                route.model = nonempty(model);
                switchboard_edit(state, slot, Some(route));
            }
        }
        #[cfg(feature = "reliquary")]
        Action::SwitchboardReasoning(slot, effort) => {
            if let Some(mut route) = route_for_edit(state, slot) {
                route.reasoning_effort = (effort != "Default").then_some(effort);
                switchboard_edit(state, slot, Some(route));
            }
        }
        #[cfg(feature = "reliquary")]
        Action::SwitchboardDimensions(value) => {
            state.switchboard_dimensions = value;
            state.switchboard_embedding_dirty = true;
            state.feedback = None;
        }
        #[cfg(feature = "reliquary")]
        Action::SwitchboardNormalization(value) => {
            state.switchboard_normalization = value;
            state.switchboard_embedding_dirty = true;
            state.feedback = None;
        }
        #[cfg(feature = "reliquary")]
        Action::SaveSwitchboard => {
            let result = (|| -> anyhow::Result<_> {
                let slot = ModelSlot::Embedding;
                let route_edited = state.switchboard_edits.iter().any(|(edited, _)| *edited == slot);
                let mut embedding_applied = false;
                if let Some(mut route) = route_for_edit(state, slot)
                    && (route_edited || state.switchboard_embedding_dirty)
                {
                    let dimensions = state.switchboard_dimensions.trim().parse::<u32>()
                        .ok().filter(|value| *value > 0)
                        .ok_or_else(|| anyhow::anyhow!("Enter the embedding model’s positive output dimension."))?;
                    let normalization = match state.switchboard_normalization.as_str() {
                        "L2" => EmbeddingNormalization::L2,
                        _ => EmbeddingNormalization::None,
                    };
                    route.embedding = Some(HostedEmbeddingSettings { dimensions, normalization });
                    switchboard_edit(state, slot, Some(route));
                    embedding_applied = true;
                }
                let saved = hermes_reliquary_plugin::save_switchboard_config(
                    lib.root(), &state.switchboard_edits,
                )?;
                Ok((saved, embedding_applied))
            })();
            state.feedback = Some(match result {
                Ok((saved, embedding_applied)) => {
                    state.switchboard_edits.clear();
                    state.switchboard = Some(saved);
                    if !state.switchboard_embedding_dirty || embedding_applied {
                        state.sync_switchboard_embedding();
                    }
                    Ok("Reliquary Switchboard settings saved.".into())
                }
                Err(error) => Err(format!("{error:#}")),
            });
        }
        // Switches and pickers save on change, as the original's autosave does.
        Action::Set(key,value,page)=>{state.edits.insert(key,value);return update(app,Action::Apply(page));}
        Action::Edit(key,value)=>{state.edits.insert(key,value);state.feedback=None;}
        Action::Apply(page)=>{
            let result=state.apply(page,lib.root());let applied=result.is_ok();state.feedback=Some(result.map(|_|"Settings saved".into()));
            if applied && page==View::Sessions{return update(app,Action::Maintain)}
        }
        Action::Browse=>return Task::perform(async {tokio::task::spawn_blocking(super::choose_folder).await.map_err(|e|e.to_string()).and_then(|r|r)},|r|Message::ProfileSetting(Action::DirectoryChosen(r))),
        Action::DirectoryChosen(Ok(Some(folder)))=>return update(app,Action::Edit("default_directory",folder.display().to_string())),
        Action::DirectoryChosen(Err(e))=>state.feedback=Some(Err(e)),
        Action::DirectoryChosen(Ok(None))=>{}
        Action::Maintain=>{
            if state.saved.auto_archive {
                let profile_root=lib.root().to_path_buf();
                let provider=app.config.clone();
                let days=state.saved.auto_archive_days;
                let current_session_id=app.current.as_ref().map(|s|s.id.clone());
                return Task::perform(async move {
                    super::local_gateway::catalog_request(
                        profile_root,provider,"catalog.archive_inactive",
                        serde_json::json!({"days":days,"current_session_id":current_session_id}),
                    ).await
                },|result|Message::ProfileSetting(Action::MaintainDone(result)));
            }
            if app.settings_view==View::Sessions{state.archived(&lib);}
            app.refresh_sessions();
        }
        Action::MaintainDone(result)=>{
            if let Err(error)=result{state.feedback=Some(Err(error));}
            if app.settings_view==View::Sessions{state.archived(&lib);}
            app.refresh_sessions();
        }
        Action::Restore(id)=>{
            if lib.session(&id).is_ok_and(|summary|summary.source==SessionSource::Native) {
                let profile_root=lib.root().to_path_buf();
                let provider=app.config.clone();
                let message_id=id.clone();
                return Task::perform(async move {
                    super::local_gateway::catalog_request(
                        profile_root,provider,"catalog.session.set",
                        serde_json::json!({"session_id":id,"archived":false}),
                    ).await
                },move|result|Message::ProfileSetting(Action::RestoreDone(message_id,result)));
            }
            state.feedback=Some(lib.set_archived(&id,false).map(|_|"Session restored".into()).map_err(|e|e.to_string()));
            state.archived(&lib);app.refresh_sessions();
        }
        Action::RestoreDone(id,result)=>{
            let succeeded=result.is_ok();
            state.feedback=Some(result.map(|_|"Session restored".into()));
            if succeeded && let Ok(summary)=lib.session(&id)
                && app.current.as_ref().is_some_and(|current|current.id==id) { app.current=Some(summary); }
            state.archived(&lib);app.refresh_sessions();
        }
        Action::Delete(id)=>state.delete=Some(id),
        Action::CancelDelete=>state.delete=None,
        Action::ConfirmDelete=>if let Some(id)=state.delete.take(){
            if lib.session(&id).is_ok_and(|summary|summary.source==SessionSource::Native) {
                let profile_root=lib.root().to_path_buf();
                let provider=app.config.clone();
                let message_id=id.clone();
                return Task::perform(async move {
                    super::local_gateway::catalog_request(
                        profile_root,provider,"catalog.session.delete",
                        serde_json::json!({"session_id":id}),
                    ).await
                },move|result|Message::ProfileSetting(Action::DeleteDone(message_id,result)));
            }
            state.feedback=Some(lib.delete_archived(&id).map(|_|"Session deleted".into()).map_err(|e|e.to_string()));
            state.archived(&lib);app.refresh_sessions();
        },
        Action::DeleteDone(_id,result)=>{
            state.feedback=Some(result.map(|_|"Session deleted".into()));
            state.archived(&lib);app.refresh_sessions();
        }
    }
    Task::none()
}

#[cfg(feature = "reliquary")]
#[derive(Clone, PartialEq)]
struct SwitchboardEndpointChoice { value: String, label: String }
#[cfg(feature = "reliquary")]
impl std::fmt::Display for SwitchboardEndpointChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.label) }
}
#[cfg(feature = "reliquary")]
fn slot_name(slot: ModelSlot) -> &'static str {
    match slot {
        ModelSlot::General => "General",
        ModelSlot::Insomnia => "Insomnia",
        ModelSlot::InsomniaMetadata => "Insomnia metadata",
        ModelSlot::EntityExtraction => "Entity extraction",
        ModelSlot::GeneralDecision => "General decision",
        ModelSlot::EntityResolutionDecision => "Entity resolution decision",
        ModelSlot::MemoryRetrievalDecision => "Memory retrieval decision",
        ModelSlot::EntityResolution => "Entity resolution",
        ModelSlot::Dream => "Dream",
        ModelSlot::Embedding => "Embeddings",
    }
}
#[cfg(feature = "reliquary")]
fn switchboard_route_row(app: &Hermes, slot: ModelSlot) -> Element<'_, Message> {
    use hermes_engine::{accounts, inference::InferenceTarget};

    let current = switchboard_route(&app.profile_settings, slot);
    let endpoint = current.map(|route| route.endpoint.as_str()).unwrap_or("");
    let no_route_label = match slot {
        ModelSlot::Embedding => "No embedding route",
        ModelSlot::General | ModelSlot::EntityExtraction | ModelSlot::EntityResolution => "No route configured",
        _ => "Use fallback",
    };
    let mut options = vec![SwitchboardEndpointChoice {
        value: String::new(),
        label: no_route_label.into(),
    }];
    if slot != ModelSlot::Embedding {
        options.extend([
            SwitchboardEndpointChoice {
                value: InferenceTarget::Chat.key(),
                label: "Current chat".into(),
            },
            SwitchboardEndpointChoice {
                value: InferenceTarget::Account("nous".into()).key(),
                label: "Nous Portal account".into(),
            },
        ]);
        options.extend(accounts::CATALOG.iter().filter_map(|account| {
            let value = InferenceTarget::Account(account.id.into()).key();
            InferenceTarget::parse(&value).ok().map(|_| SwitchboardEndpointChoice {
                value,
                label: account.name.into(),
            })
        }));
    }
    options.extend(app.models.endpoints.iter().map(|saved| SwitchboardEndpointChoice {
        value: InferenceTarget::Endpoint(saved.id.clone()).key(),
        label: saved.name.clone(),
    }));
    if !endpoint.is_empty() && !options.iter().any(|choice| choice.value == endpoint) {
        options.push(SwitchboardEndpointChoice {
            value: endpoint.into(),
            label: "Unavailable route".into(),
        });
    }
    let selected = options.iter().find(|choice| choice.value == endpoint).cloned();
    let picker = pick_list(options, selected, move |choice| {
        Message::ProfileSetting(Action::SwitchboardEndpoint(slot, choice.value))
    }).text_size(11).width(165);
    let mut controls = row![picker].spacing(5).align_y(Alignment::Center);
    if let Some(route) = current {
        let model = text_input("Model override (optional)", route.model.as_deref().unwrap_or(""))
            .size(11)
            .padding([5, 7])
            .width(165)
            .on_input(move |value| Message::ProfileSetting(Action::SwitchboardModel(slot, value)));
        controls = controls.push(model);
        if slot != ModelSlot::Embedding && !slot.is_decision() {
            let choices = ["Default", "none", "minimal", "low", "medium", "high", "xhigh", "max"]
                .into_iter().map(str::to_owned).collect::<Vec<_>>();
            let selected = route.reasoning_effort.clone().unwrap_or_else(|| "Default".into());
            let reasoning = pick_list(choices, Some(selected), move |value| {
                Message::ProfileSetting(Action::SwitchboardReasoning(slot, value))
            }).text_size(11).width(90);
            controls = controls.push(reasoning);
        }
    }
    let description = match slot {
        ModelSlot::General => "Default completion route for Reliquary. Initially uses the Hermes chat model.",
        ModelSlot::EntityResolution => "Required for completed-turn processing. Initially uses the Hermes chat model.",
        ModelSlot::EntityExtraction => "Optional route for entity extraction.",
        ModelSlot::Embedding if app.models.endpoints.is_empty() => "Add a saved OpenAI-compatible endpoint under Models.",
        ModelSlot::Embedding => "Choose the endpoint used for indexing and recall.",
        _ => "Optional override. Reliquary applies its fallback when this slot is unset.",
    };
    super::settings::card_row(slot_name(slot), description, controls.into(), app.dark)
}

#[cfg(feature = "reliquary")]
fn switchboard_rows(app: &Hermes) -> Vec<Element<'_, Message>> {
    let mut rows = ModelSlot::ALL.into_iter()
        .map(|slot| switchboard_route_row(app, slot))
        .collect::<Vec<_>>();
    let state = &app.profile_settings;
    let dimensions = text_input("Dimensions", &state.switchboard_dimensions)
        .size(11)
        .padding([5, 7])
        .width(90)
        .on_input(|value| Message::ProfileSetting(Action::SwitchboardDimensions(value)));
    let normalization = if state.switchboard_normalization.is_empty() {
        "None".into()
    } else {
        state.switchboard_normalization.clone()
    };
    let norm = pick_list(vec!["None".to_owned(), "L2".into()], Some(normalization), |value| {
        Message::ProfileSetting(Action::SwitchboardNormalization(value))
    }).text_size(11).width(90);
    let save = button(text("Save Switchboard").size(12))
        .on_press(Message::ProfileSetting(Action::SaveSwitchboard));
    let controls = row![dimensions, norm, save].spacing(6).align_y(Alignment::Center);
    rows.push(super::settings::card_row(
        "Embedding settings",
        "Set the model’s output dimensions and normalization.",
        controls.into(),
        app.dark,
    ));
    rows
}

pub(super) fn view(app:&Hermes,page:View)->Element<'_,Message>{
    let p=ui::palette(app.dark);let state=&app.profile_settings;
    let title=match page {View::Chat=>"Behavior",View::Workspace=>"Projects & files",View::Memory=>"Persistent memory",View::Safety=>"Approvals",View::Advanced=>"Runtime & tool output",View::Sessions=>"Sessions",_=>""};
    let dark=app.dark;
    let mut body=column![super::settings::section_title(title,dark)].spacing(8);
    let mut rows=Vec::new();
    if page == View::Memory {
        use MemoryProvider::{Auto, External, Notes, Off, Reliquary};
        let saved = &state.saved;
        let stock = hermes_gateway::stock_plugins::has_memory_plugin();
        let description = match saved.memory_provider {
            Auto if stock => "Use Reliquary if this profile is already enrolled. Otherwise use its Markdown notes.",
            Auto => "Use this profile’s Markdown notes.",
            Reliquary if stock => "Enroll this profile with the Reliquary stock plugin. New chats can capture turns and query their assigned memory.",
            Reliquary => "This build does not include the Reliquary stock plugin.",
            Notes => "Store agent and user notes in this profile’s Markdown files.",
            External if !saved.external_memory_connector =>
                r#"In Capabilities > Connectors, configure your MCP server and select "Use as external memory provider"."#,
            External if !saved.mcp_tools || !saved.memory_tool =>
                "Enable both MCP tools and Memory tools in Advanced to use the external provider.",
            External => "Use tools from the MCP server designated for memory in Capabilities > Connectors.",
            Off => "Disable built-in memory and the designated external memory connector for this profile.",
        };
        let options = if stock || saved.memory_provider == Reliquary {
            vec![Auto, Reliquary, Notes, External, Off]
        } else {
            vec![Auto, Notes, External, Off]
        };
        let selector = pick_list(options, Some(saved.memory_provider),
            |choice| Message::ProfileSetting(Action::SetMemoryProvider(choice)))
            .width(165).text_size(13).padding([7, 10]);
        rows.push(super::settings::card_row("Memory provider", description, selector.into(), dark));
        if stock && matches!(saved.memory_provider, Reliquary | Auto) {
            #[cfg(feature = "reliquary")]
            rows.extend(switchboard_rows(app));
        }
    }
    for f in FIELDS.iter().filter(|f|f.page==page){
        if page == View::Memory && !matches!(state.saved.memory_provider, MemoryProvider::Auto | MemoryProvider::Notes) {
            continue;
        }
        let key=f.key;let value=state.value(f);
        let control:Element<'_,Message>=match f.kind {
            Kind::Check=>super::settings::switch(value=="true",move|b|Message::ProfileSetting(Action::Set(key,b.to_string(),page)),dark),
            Kind::Personality=>{
                let options=std::iter::once("default".to_owned()).chain(PERSONALITIES.keys().cloned()).collect::<Vec<_>>();
                pick_list(options,Some(value),move|s|Message::ProfileSetting(Action::Set(key,s,page))).width(180).text_size(13).padding([7,10]).into()
            }
            _=>{
                let input=text_input(if key=="timezone"{"System default"}else{""},&value).size(13).padding([7,10]).width(if f.kind==Kind::Count{110}else{220})
                .style(super::settings::field_style(dark))
                .on_input(move|s|Message::ProfileSetting(Action::Edit(key,s))).on_submit(Message::ProfileSetting(Action::Apply(page)));
                if f.kind==Kind::Directory {row![input,button(text("Browse…").size(13)).padding([7,10]).on_press(Message::ProfileSetting(Action::Browse))].spacing(6).into()}else{input.into()}
            }
        };
        rows.push(super::settings::card_row(f.label,f.description,control,dark));
    }
    body=body.push(super::settings::card(rows,dark));
    // Text fields save on Enter; Apply is offered only while one has unsaved edits.
    let dirty=FIELDS.iter().any(|f|f.page==page && matches!(f.kind,Kind::Count|Kind::Text|Kind::Directory) && state.edits.contains_key(f.key));
    if dirty {body=body.push(row![text("Unsaved changes").size(12).color(p.muted),Space::new().width(Fill),button(text("Save").size(13)).padding([6,14]).on_press(Message::ProfileSetting(Action::Apply(page)))].align_y(Alignment::Center));}
    if let Some(result)=&state.feedback{body=body.push(match result {Ok(s)=>text(s).color(p.muted),Err(s)=>text(s).color(iced::Color::from_rgb8(194,61,64))}.size(12));}
    if page==View::Workspace {body=body.push(Space::new().height(12)).push(super::project_discovery::view(app));}
    if page==View::Safety {body=body.push(super::checkpoint_ui::view(app));}
    if page==View::Sessions{
        body=body.push(text("Archived sessions").size(16));
        if state.archive.is_empty(){body=body.push(text("No archived sessions").size(13).color(p.muted));}
        for chat in &state.archive{
            let (restore,remove)=(chat.id.clone(),chat.id.clone());
            body=body.push(row![text(&chat.title).size(13).width(Fill),
                button(text("Restore").size(12)).on_press(Message::ProfileSetting(Action::Restore(restore))),
                button(text("Delete…").size(12)).on_press(Message::ProfileSetting(Action::Delete(remove)))].spacing(6));
            if state.delete.as_ref()==Some(&chat.id) {
                body=body.push(row![text("Permanently delete this archived conversation?").size(12),
                button(text("Cancel").size(12)).on_press(Message::ProfileSetting(Action::CancelDelete)),
                button(text("Delete permanently").size(12)).on_press(Message::ProfileSetting(Action::ConfirmDelete))].spacing(6));
            }
        }
        body=body.push(button(text("Import sessions…").size(13)).on_press(Message::Navigate(super::Screen::Import)));
    }
    super::settings::settings_content(container(body).width(Fill).into())
}

#[cfg(test)]
mod switchboard_tests {
    use super::effective_route;

    #[test]
    fn staged_clear_hides_saved_route_and_staged_route_replaces_it() {
        let saved = String::from("saved");
        let pending = String::from("pending");
        assert!(effective_route(Some(None), Some(&saved)).is_none());
        assert_eq!(effective_route(None, Some(&saved)), Some(&saved));
        assert_eq!(effective_route(Some(Some(&pending)), Some(&saved)), Some(&pending));
    }
}
