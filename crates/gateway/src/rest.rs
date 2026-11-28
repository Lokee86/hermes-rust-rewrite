//! Stock client read routes over this gateway's profile and canonical transcript.
use super::*;
use auth::{Access, error, protected};
use axum::extract::Path as RoutePath;
use hermes_library::{ArchiveFilter, SessionQuery, SessionSource};

pub(super) fn profile_matches(shared: &Shared, profile: Option<&str>, allow_all: bool) -> bool {
    profile.is_none_or(|name| {
        matches!(name, "" | "current")
            || name == shared.profile_name
            || name == shared.profile_id
            || (allow_all && name == "all")
    })
}

pub(super) async fn profiles(_: Access, State(shared): State<Arc<Shared>>) -> Response {
    protected(json!({"profiles":[{
        "name":shared.profile_name,"path":shared.library.root(),"is_default":shared.profile_name=="default",
        "model":shared.provider.model,"provider":provider_label(&shared.provider),
        "has_env":!shared.provider.api_key.is_empty(),"skill_count":0,"gateway_running":true,
        "description":"","description_auto":false,"display_name":"","bot_title":"","ui_meta":{},
        "distribution_name":null,"distribution_version":null,"distribution_source":null,"has_alias":false
    }]}))
}

pub(super) async fn active(_: Access, State(shared): State<Arc<Shared>>) -> Response {
    protected(json!({"active":shared.profile_name,"current":shared.profile_name}))
}

#[derive(Clone, Default, Deserialize)]
pub(super) struct ListQuery {
    profile: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    min_messages: Option<i64>,
    archived: Option<String>,
    order: Option<String>,
    source: Option<String>,
    sources: Option<String>,
    exclude_sources: Option<String>,
    cwd_prefix: Option<String>,
}

fn csv(value: Option<&str>) -> impl Iterator<Item = &str> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn source_for(summary: &SessionSummary, settings: &Settings) -> String {
    // Stock clients name scheduled-job chats `cron`: the cron sidebar selects them by that name,
    // and recents leave them out by it.
    if settings.source == "scheduled" {
        "cron".into()
    } else if settings.source.is_empty() {
        match &summary.source {
            SessionSource::Hermes { .. } => "hermes",
            SessionSource::Native | SessionSource::LegacyRust => "rust",
        }
        .into()
    } else {
        settings.source.clone()
    }
}

async fn session_rows(
    shared: &Arc<Shared>,
    query: &ListQuery,
    archive: ArchiveFilter,
    recent: bool,
) -> Result<Vec<Value>> {
    let live = shared
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, session)| (id.clone(), Arc::downgrade(session)))
        .collect();
    let shared = Arc::clone(shared);
    let query = query.clone();
    tokio::task::spawn_blocking(move || read_rows(&shared, &query, archive, recent, live)).await?
}

fn read_rows(
    shared: &Shared,
    query: &ListQuery,
    archive: ArchiveFilter,
    recent: bool,
    live: HashMap<String, std::sync::Weak<LiveSession>>,
) -> Result<Vec<Value>> {
    let project_paths: HashMap<_, _> = shared
        .library
        .projects()?
        .into_iter()
        .map(|project| (project.id, project.path))
        .collect();
    let mut catalog = shared.library.list_sessions(&SessionQuery {
        archive,
        ..Default::default()
    })?;
    // The catalog already preserves pin ordering. Use a stable sort within the unpinned partition.
    if !recent {
        catalog.sort_by(|left, right| {
            right.pinned.cmp(&left.pinned).then_with(|| {
                if left.pinned && right.pinned {
                    std::cmp::Ordering::Equal
                } else {
                    right.created_at.cmp(&left.created_at)
                }
            })
        });
    }
    let sources: Vec<_> = csv(query.sources.as_deref()).collect();
    let excluded: Vec<_> = csv(query.exclude_sources.as_deref()).collect();
    let min_messages = query.min_messages.unwrap_or(0).max(0) as u64;
    let mut rows = Vec::new();
    for summary in catalog {
        // One unreadable conversation is left out rather than failing the whole listing.
        let Ok(settings) = Settings::read(&summary.directory) else {
            continue;
        };
        let source = source_for(&summary, &settings);
        let source_match = if let Some(exact) = query.source.as_deref().filter(|s| !s.is_empty()) {
            exact == source
        } else {
            sources.is_empty() || sources.contains(&source.as_str())
        };
        if !source_match || excluded.contains(&source.as_str()) {
            continue;
        }
        let cwd = summary
            .project_id
            .as_ref()
            .and_then(|id| project_paths.get(id))
            .cloned()
            .or(settings.cwd);
        if query
            .cwd_prefix
            .as_deref()
            .filter(|s| !s.is_empty())
            .is_some_and(|prefix| cwd.as_ref().is_none_or(|cwd| !cwd.starts_with(prefix)))
        {
            continue;
        }
        let (history, running) = match live.get(&summary.id).and_then(std::sync::Weak::upgrade) {
            Some(session) => {
                let view = lock(&session.view);
                (view.history.clone(), view.running)
            }
            None => match read_history(&summary.directory) {
                Ok(history) => (history, false),
                Err(_) => continue,
            },
        };
        let count = history
            .turns
            .iter()
            .map(|turn| 1 + turn.entries.len())
            .sum::<usize>();
        if (count as u64) < min_messages {
            continue;
        }
        let preview = history.turns.first().map(|t| shorten(&t.prompt, 160));
        let last_reply = history
            .turns
            .iter()
            .rev()
            .flat_map(|turn| turn.entries.iter().rev())
            .find_map(|entry| match entry {
                Entry::Assistant { text, .. } if !text.is_empty() => Some(shorten(text, 160)),
                _ => None,
            });
        let tool_call_count = history
            .turns
            .iter()
            .flat_map(|turn| &turn.entries)
            .map(|entry| match entry {
                Entry::Assistant { calls, .. } => calls.len(),
                _ => 0,
            })
            .sum::<usize>();
        rows.push(json!({
            "id":summary.id,"title":summary.title,"source":source,"profile":shared.profile_name,
            "project_id":summary.project_id,"is_default_profile":shared.profile_name=="default",
            "started_at":epoch_time(&summary.created_at),"last_active":epoch_time(summary.recency()),
            "ended_at":null,"message_count":count,"tool_call_count":tool_call_count,
            "preview":preview,"last_reply_preview":last_reply,"model":settings.model.as_ref().unwrap_or(&shared.provider.model),
            "cwd":cwd,"is_active":running,"archived":summary.archived,"pinned":summary.pinned
        }));
    }
    Ok(rows)
}

async fn list(shared: Arc<Shared>, query: ListQuery, aggregated: bool) -> Response {
    if !profile_matches(&shared, query.profile.as_deref(), aggregated) {
        return error(
            StatusCode::FORBIDDEN,
            "This gateway serves a different profile",
        );
    }
    let limit = query.limit.unwrap_or(20);
    if limit > if aggregated { 500 } else { 100 } {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Limit exceeds the maximum for this route",
        );
    }
    let offset = query.offset.unwrap_or(0);
    let archive = match query.archived.as_deref().unwrap_or("exclude") {
        "exclude" => ArchiveFilter::Active,
        "only" => ArchiveFilter::Archived,
        "include" => ArchiveFilter::All,
        _ => {
            return error(
                StatusCode::BAD_REQUEST,
                "archived must be one of: exclude, only, include",
            );
        }
    };
    let recent =
        match query
            .order
            .as_deref()
            .unwrap_or(if aggregated { "recent" } else { "created" })
        {
            "recent" => true,
            "created" => false,
            _ => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "order must be one of: created, recent",
                );
            }
        };
    let Ok(rows) = session_rows(&shared, &query, archive, recent).await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Session store unavailable");
    };
    let total = rows.len();
    let mut page = pinned_page(&rows, offset, limit);
    if archive == ArchiveFilter::Archived {
        let pins = session_rows(&shared, &query, ArchiveFilter::Active, recent).await;
        let Ok(pins) = pins else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "Session store unavailable");
        };
        page.extend(pins.into_iter().filter(|row| row["pinned"] == true));
    }
    let mut result =
        json!({"sessions":page,"total":total,"limit":limit,"offset":offset,"storage":{}});
    if aggregated {
        result["profile_totals"] = json!({&shared.profile_name:total});
        result["errors"] = json!([]);
    }
    protected(result)
}

fn pinned_page(rows: &[Value], offset: usize, limit: usize) -> Vec<Value> {
    let mut page: Vec<_> = rows.iter().skip(offset).take(limit).cloned().collect();
    if offset == 0 && limit > 0 {
        page.extend(
            rows.iter()
                .skip(limit)
                .take_while(|row| row["pinned"] == true)
                .cloned(),
        );
    }
    page
}

pub(super) async fn sessions(
    _: Access,
    State(shared): State<Arc<Shared>>,
    Query(query): Query<ListQuery>,
) -> Response {
    list(shared, query, false).await
}

pub(super) async fn profile_sessions(
    _: Access,
    State(shared): State<Arc<Shared>>,
    Query(query): Query<ListQuery>,
) -> Response {
    list(shared, query, true).await
}

#[derive(Deserialize)]
pub(super) struct SidebarQuery {
    profile: Option<String>,
    recents_profile: Option<String>,
    recents_limit: Option<i64>,
    recents_exclude: Option<String>,
    cron_limit: Option<i64>,
    messaging_limit: Option<i64>,
    messaging_exclude: Option<String>,
}

pub(super) async fn sidebar(
    _: Access,
    State(shared): State<Arc<Shared>>,
    Query(query): Query<SidebarQuery>,
) -> Response {
    if !profile_matches(&shared, query.profile.as_deref(), true)
        || !profile_matches(&shared, query.recents_profile.as_deref(), true)
    {
        return error(
            StatusCode::FORBIDDEN,
            "This gateway serves a different profile",
        );
    }
    let q = ListQuery {
        min_messages: Some(1),
        ..Default::default()
    };
    let Ok(rows) = session_rows(&shared, &q, ArchiveFilter::Active, true).await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "Session store unavailable");
    };
    let recents_excluded: Vec<_> = csv(query.recents_exclude.as_deref()).collect();
    let messaging_excluded: Vec<_> = csv(query.messaging_exclude.as_deref()).collect();
    let recents: Vec<_> = rows
        .iter()
        .filter(|r| {
            r["source"]
                .as_str()
                .is_some_and(|s| !recents_excluded.contains(&s))
        })
        .cloned()
        .collect();
    let cron: Vec<_> = rows
        .iter()
        .filter(|r| r["source"] == "cron")
        .cloned()
        .collect();
    let messaging: Vec<_> = rows
        .into_iter()
        .filter(|r| {
            r["source"]
                .as_str()
                .is_some_and(|s| !messaging_excluded.contains(&s))
        })
        .collect();
    let recents_limit = query.recents_limit.unwrap_or(20).clamp(1, 500) as usize;
    protected(json!({
        "recents":{"sessions":pinned_page(&recents, 0, recents_limit),
            "profiles_truncated":{&shared.profile_name:recents.len()>=recents_limit},"profiles_usage":{}},
        "cron":{"sessions":pinned_page(&cron, 0, query.cron_limit.unwrap_or(50).clamp(1, 500) as usize)},
        "messaging":{"sessions":pinned_page(&messaging, 0, query.messaging_limit.unwrap_or(100).clamp(1, 500) as usize),
            "total":messaging.len()},"errors":[],"storage":{}
    }))
}

#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    profile: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    order: Option<String>,
}

pub(super) async fn history(
    _: Access,
    State(shared): State<Arc<Shared>>,
    RoutePath(requested): RoutePath<String>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    if !profile_matches(&shared, query.profile.as_deref(), false) {
        return error(
            StatusCode::FORBIDDEN,
            "This gateway serves a different profile",
        );
    }
    let order = query.order.as_deref().unwrap_or(if query.limit.is_some() {
        "oldest"
    } else {
        "latest"
    });
    if !matches!(order, "oldest" | "latest") {
        return error(
            StatusCode::BAD_REQUEST,
            "order must be one of: oldest, latest",
        );
    }
    let summary = if let Ok(summary) = shared.library.session(&requested) {
        summary
    } else {
        let Ok(rows) = shared.library.list_sessions(&SessionQuery {
            archive: ArchiveFilter::All,
            ..Default::default()
        }) else {
            return error(StatusCode::SERVICE_UNAVAILABLE, "Session store unavailable");
        };
        let mut matches = rows
            .into_iter()
            .filter(|row| row.id.starts_with(&requested));
        match (matches.next(), matches.next()) {
            (Some(row), None) => row,
            _ => return error(StatusCode::NOT_FOUND, "Session not found"),
        }
    };
    let id = summary.id;
    let live = shared.sessions.lock().await.get(&id).cloned();
    let messages = if let Some(session) = live {
        let view = lock(&session.view);
        transcript(&id, &view.history, Some(&view.authority))
    } else {
        let result = shared.library.session_dir(&id).and_then(|directory| {
            read_record(&directory)
                .map(|record| transcript(&id, &record.history, record.authority.as_ref()))
        });
        match result {
            Ok(messages) => messages,
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "Session store unavailable"),
        }
    };
    let limit = query.limit.unwrap_or(500).min(500);
    let offset = query.offset.unwrap_or(0);
    let (start, end) = if order == "latest" {
        let end = messages.len().saturating_sub(offset);
        (end.saturating_sub(limit), end)
    } else {
        let start = offset.min(messages.len());
        (start, start.saturating_add(limit).min(messages.len()))
    };
    let page: Vec<_> = messages
        .into_iter()
        .skip(start)
        .take(end - start)
        .map(|mut message| {
            message["id"] = message["row_id"].clone();
            message["session_id"] = json!(id);
            message["message_uid"] = message["message_id"].clone();
            message["active"] = json!(1);
            message["compacted"] = json!(0);
            if message.get("timestamp").is_none() {
                message["timestamp"] = json!(epoch_time(&summary.created_at));
            }
            if let Some(name) = message.get("name").cloned() {
                message["tool_name"] = name;
            }
            message
        })
        .collect();
    protected(
        json!({"session_id":id,"profile":shared.profile_name,"messages":page,
        "pagination":{"limit":limit,"offset":offset,"order":order,"returned":end-start}}),
    )
}
