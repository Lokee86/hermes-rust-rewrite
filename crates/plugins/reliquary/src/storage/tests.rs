use super::*;
use hermes_engine::memory_capture::CaptureBinding;
use serde_json::{Value, json};
use std::{sync::mpsc, thread, time::Duration};

fn legacy_profile(temp: &Path) -> (Library, PathBuf, OwnerId, OwnerId, OwnerId) {
    let catalog = Library::open(temp.join("data")).unwrap();
    let profile = catalog.create_profile("work").unwrap();
    let directory = profile.path.join("reliquary");
    fs::create_dir_all(&directory).unwrap();
    let general = Rel::create(directory.join("general.rel")).unwrap();
    let project = Rel::create(directory.join("project.rel")).unwrap();
    let phy = Phylactery::create(directory.join("work.phy")).unwrap();
    let external_path = temp.join("external.rel");
    let external = Rel::create(&external_path).unwrap();
    let general_path = general.path().to_str().unwrap();
    let general_path = general_path.strip_prefix(r"\\?\").unwrap_or(general_path);
    let owners = json!({
        general.owner_id().to_string(): {"kind":"rel", "path":general_path},
        project.owner_id().to_string(): {"kind":"rel", "path":project.path()},
        phy.owner_id().to_string(): {"kind":"phylactery", "path":phy.path()},
        external.owner_id().to_string(): {"kind":"rel", "path":external_path},
    });
    // This is the pre-managed v2 topology: profile IDs and absolute owner paths
    // are established before any `reliquary-storage.json` locator exists.
    let topology = json!({
        "version": 2,
        "profile_id": "work",
        "principal_id": "legacy-principal",
        "main_rel": general.owner_id(),
        "active_phy": phy.owner_id(),
        "projects": {"project-id": {"rel_id": project.owner_id()}},
        "owners": owners,
    });
    fs::write(
        directory.join(TOPOLOGY),
        serde_json::to_vec(&topology).unwrap(),
    )
    .unwrap();
    fs::write(
        directory.join(ENROLLMENT),
        serde_json::to_vec(&Enrollment {
            version: 1,
            cutoff: now(),
        })
        .unwrap(),
    )
    .unwrap();
    (
        catalog,
        profile.path,
        general.owner_id(),
        project.owner_id(),
        phy.owner_id(),
    )
}

fn legacy_topology(home: &Path) -> Value {
    serde_json::from_slice(&fs::read(home.join("reliquary").join(TOPOLOGY)).unwrap()).unwrap()
}

fn assert_legacy_binding(memory: &EmbeddedMemory, general: OwnerId, phy: OwnerId) {
    let binding = CaptureBinding {
        profile_id: "work".into(),
        principal_id: "legacy-principal".into(),
        project_id: None,
        rel_id: general.to_string(),
        phy_id: phy.to_string(),
    };
    assert_eq!(memory.route(&binding).unwrap().0, general);
}

#[test]
fn renaming_pre_managed_profile_rebases_only_profile_owned_paths() {
    let temp = tempfile::tempdir().unwrap();
    let (catalog, old_home, general, project, phy) = legacy_profile(temp.path());
    let external_path = temp.path().join("external.rel");
    let external_id = Rel::open(&external_path).unwrap().owner_id();
    let old_topology = legacy_topology(&old_home);

    assert_eq!(rename_profile(&catalog, "work", "office").unwrap(), None);

    let new_home = catalog.profile_path("office").unwrap();
    assert!(!old_home.exists());
    let topology = legacy_topology(&new_home);
    for field in ["profile_id", "principal_id", "main_rel", "active_phy"] {
        assert_eq!(topology[field], old_topology[field], "{field} changed");
    }
    assert_eq!(topology["profile_id"], "work");
    assert_eq!(
        topology["owners"][general.to_string()]["path"],
        new_home
            .join("reliquary/general.rel")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        topology["owners"][project.to_string()]["path"],
        new_home
            .join("reliquary/project.rel")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        topology["owners"][phy.to_string()]["path"],
        new_home
            .join("reliquary/work.phy")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        topology["owners"][external_id.to_string()]["path"],
        external_path.to_string_lossy().as_ref()
    );

    let library = Library::open(&new_home).unwrap();
    let adjacent = temp.path().join("unused-adjacent");
    let memory = open_at(&library, "office", false, &adjacent)
        .unwrap()
        .unwrap();
    assert_legacy_binding(&memory, general, phy);
    assert_eq!(Rel::open(&external_path).unwrap().owner_id(), external_id);
}

#[test]
fn opening_renamed_legacy_profile_recovers_interrupted_path_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let (catalog, old_home, general, _project, phy) = legacy_profile(temp.path());
    let directory = old_home.join("reliquary");
    let intent = LegacyRelocation {
        version: 1,
        profile_id: "work".into(),
        current_name: "work".into(),
        pending: Some(LegacyProfileMove {
            from_home: old_home.clone(),
            to_home: catalog.profile_path("office").unwrap(),
            to_name: "office".into(),
        }),
    };
    save_state(&legacy_relocation_path(&directory), &intent, false).unwrap();
    catalog.rename_profile("work", "office").unwrap();

    let new_home = catalog.profile_path("office").unwrap();
    let library = Library::open(&new_home).unwrap();
    let memory = open_at(
        &library,
        "office",
        false,
        &temp.path().join("unused-adjacent"),
    )
    .unwrap()
    .unwrap();
    assert_legacy_binding(&memory, general, phy);
    let topology = legacy_topology(&new_home);
    assert_eq!(topology["profile_id"], "work");
    assert_eq!(
        topology["owners"][general.to_string()]["path"],
        new_home
            .join("reliquary/general.rel")
            .to_string_lossy()
            .as_ref()
    );
    let relocation = load_legacy_relocation(&new_home.join("reliquary"))
        .unwrap()
        .unwrap();
    assert_eq!(relocation.current_name, "office");
    assert!(relocation.pending.is_none());
}

#[test]
fn legacy_open_waits_for_parent_rename_fence_before_recovering_intent() {
    let temp = tempfile::tempdir().unwrap();
    let (catalog, old_home, _general, _project, _phy) = legacy_profile(temp.path());
    let directory = old_home.join("reliquary");
    let intent = LegacyRelocation {
        version: 1,
        profile_id: "work".into(),
        current_name: "work".into(),
        pending: Some(LegacyProfileMove {
            from_home: old_home.clone(),
            to_home: catalog.profile_path("office").unwrap(),
            to_name: "office".into(),
        }),
    };
    save_state(&legacy_relocation_path(&directory), &intent, false).unwrap();
    let fence = legacy_rename_lock(&old_home).unwrap().unwrap();
    let (send, receive) = mpsc::channel();
    let home = old_home.clone();
    let adjacent = temp.path().join("unused-adjacent");
    let open = thread::spawn(move || {
        let library = Library::open(&home).unwrap();
        let opened = open_at(&library, "work", false, &adjacent)
            .unwrap()
            .is_some();
        send.send(opened).unwrap();
    });

    assert!(receive.recv_timeout(Duration::from_millis(100)).is_err());
    drop(fence);
    assert!(receive.recv_timeout(Duration::from_secs(5)).unwrap());
    open.join().unwrap();
    let relocation = load_legacy_relocation(&directory).unwrap().unwrap();
    assert!(relocation.pending.is_none());
}

#[test]
fn same_names_get_distinct_owners_and_paths() {
    let temp = tempfile::tempdir().unwrap();
    let a = Library::open(temp.path().join("a")).unwrap();
    let b = Library::open(temp.path().join("b")).unwrap();
    let root = temp.path().join("reliquary");
    let first = temp.path().join("one").join("Design");
    let second = temp.path().join("two").join("Design");
    for project in [&first, &second] {
        fs::create_dir_all(project).unwrap();
    }
    let one = a.add_project(&first).unwrap();
    let two = a.add_project(&second).unwrap();
    assert!(open_at(&a, "default", false, &root).unwrap().is_none());
    let a_memory = open_at(&a, "default", true, &root).unwrap().unwrap();
    let b_memory = open_at(&b, "default", true, &root).unwrap().unwrap();
    let location = Location::load(a.root()).unwrap().unwrap();
    let other = Location::load(b.root()).unwrap().unwrap();
    assert_ne!(location.id, other.id);
    assert_ne!(location.folder, other.folder);
    let directory = location.profile_directory();
    let topology = Topology::load(&directory, &location.profile_id(), true).unwrap();
    let one_id = topology.projects[&one.id].rel_id;
    let two_id = topology.projects[&two.id].rel_id;
    assert_ne!(one_id, two_id);
    assert_eq!(
        topology.owners[&one_id].path,
        Path::new("projects/Design.rel")
    );
    assert_ne!(topology.owners[&one_id].path, topology.owners[&two_id].path);
    let foreign = Topology::load(&other.profile_directory(), &other.profile_id(), true).unwrap();
    assert_ne!(topology.main_rel, foreign.main_rel);
    assert_ne!(topology.active_phy, foreign.active_phy);
    drop((a_memory, b_memory));

    let copied = Library::open(temp.path().join("copied")).unwrap();
    ensure_directory(&copied.root().join("plugin-data")).unwrap();
    fs::copy(location_path(a.root()), location_path(copied.root())).unwrap();
    assert!(open_at(&copied, "default", false, &root).unwrap().is_none());
    drop(open_at(&copied, "default", true, &root).unwrap().unwrap());
    let copy = Location::load(copied.root()).unwrap().unwrap();
    assert_ne!(location.id, copy.id);
    assert_ne!(location.folder, copy.folder);
    assert_eq!(Location::load(a.root()).unwrap().unwrap().id, location.id);
}

#[test]
fn resumes_renames_between_the_file_move_and_registry_commit() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = Library::open(temp.path().join("profiles-root")).unwrap();
    let profile = catalog.create_profile("work").unwrap();
    let library = Library::open(profile.path).unwrap();
    let folder = temp.path().join("Design");
    fs::create_dir(&folder).unwrap();
    let project = library.add_project(&folder).unwrap();
    let root = temp.path().join("reliquary");

    // Recover a profile which published its enrollment intent before the plug-in opened.
    let mut location = provision(library.root(), "work", &root).unwrap();
    let staging = ".enroll-recovery".to_owned();
    fs::rename(location.profile_directory(), location.root.join(&staging)).unwrap();
    location.staging_folder = Some(staging);
    save_state(&location_path(library.root()), &location, true).unwrap();
    drop(open_at(&library, "work", false, &root).unwrap().unwrap());
    location = Location::load(library.root()).unwrap().unwrap();
    assert!(location.staging_folder.is_none());
    let directory = location.profile_directory();
    let mut topology = Topology::load(&directory, &location.profile_id(), true).unwrap();
    let id = topology.projects[&project.id].rel_id;
    let owner = topology.owners.get_mut(&id).unwrap();
    let new = PathBuf::from("projects").join("_AUX.rel");
    owner.pending_path = Some(new.clone());
    fs::rename(directory.join(&owner.path), directory.join(&new)).unwrap();
    save_state(&directory.join(TOPOLOGY), &topology, true).unwrap();
    library.rename_project(&project.id, "AUX").unwrap();

    let new_home = catalog.profile_path("office").unwrap();
    location.rename = Some(ProfileRename {
        home: new_home.clone(),
        folder: "office".into(),
    });
    save_state(&location_path(library.root()), &location, true).unwrap();
    catalog.rename_profile("work", "office").unwrap();
    fs::rename(directory, location.root.join("office")).unwrap();

    let office = Library::open(new_home).unwrap();
    drop(open_at(&office, "office", false, &root).unwrap().unwrap());
    let directory = root.join("office");
    assert_eq!(Rel::open(directory.join(new)).unwrap().owner_id(), id);
    assert_eq!(
        Phylactery::open(directory.join("office.phy"))
            .unwrap()
            .owner_id(),
        topology.active_phy
    );
    let resumed = Topology::load(
        &fs::canonicalize(directory).unwrap(),
        &location.profile_id(),
        true,
    )
    .unwrap();
    assert!(resumed.owners[&id].pending_path.is_none());
}

#[test]
fn profile_deletion_resumes_after_detach_and_partial_purge() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = Library::open(temp.path()).unwrap();

    for (name, progress) in [
        ("detaching", deletion::Progress::Detach),
        ("purging", deletion::Progress::Purge),
    ] {
        let profile = Library::open(catalog.create_profile(name).unwrap().path).unwrap();
        drop(open(&profile, name, true).unwrap().unwrap());
        let home = profile.root();
        let mut location = Location::load(home).unwrap().unwrap();
        let detached = location
            .root
            .join(format!(".delete-{}", location.profile_id()));
        fs::rename(location.profile_directory(), &detached).unwrap();
        location.deletion = Some(progress);
        save_state(&location_path(home), &location, true).unwrap();
        if progress == deletion::Progress::Purge {
            fs::remove_file(detached.join(TOPOLOGY)).unwrap();
        }

        assert!(open(&profile, name, true).is_err());
        purge_profile_data(home, name).unwrap();
        purge_profile_data(home, name).unwrap();
        assert!(!detached.exists());
        assert!(location_path(home).is_file());
    }
}

#[test]
fn profile_deletion_recovers_a_move_lost_after_purge_was_recorded() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = Library::open(temp.path()).unwrap();
    let profile = Library::open(catalog.create_profile("work").unwrap().path).unwrap();
    drop(open(&profile, "work", true).unwrap().unwrap());
    let home = profile.root();
    let mut location = Location::load(home).unwrap().unwrap();
    let directory = location.profile_directory();
    location.deletion = Some(deletion::Progress::Purge);
    save_state(&location_path(home), &location, true).unwrap();

    purge_profile_data(home, "work").unwrap();
    assert!(!directory.exists());

    let replacement = Library::open(catalog.create_profile("replacement").unwrap().path).unwrap();
    drop(open(&replacement, "work", true).unwrap().unwrap());
    let replacement_location = Location::load(replacement.root()).unwrap().unwrap();
    assert_eq!(replacement_location.profile_directory(), directory);
    purge_profile_data(home, "work").unwrap();
    assert!(directory.join("general.rel").is_file());
}

#[test]
fn profile_deletion_requires_the_registered_identity_and_respects_a_mount() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = Library::open(temp.path()).unwrap();
    let original = Library::open(catalog.create_profile("original").unwrap().path).unwrap();
    let mounted = open(&original, "original", true).unwrap().unwrap();
    let mut location = Location::load(original.root()).unwrap().unwrap();
    let directory = location.profile_directory();

    assert!(purge_profile_data(original.root(), "original").is_err());
    assert!(directory.join("general.rel").is_file());
    drop(mounted);

    let imported = Library::open(catalog.create_profile("imported").unwrap().path).unwrap();
    ensure_directory(&imported.root().join("plugin-data")).unwrap();
    fs::copy(
        location_path(original.root()),
        location_path(imported.root()),
    )
    .unwrap();
    purge_profile_data(imported.root(), "imported").unwrap();
    assert!(directory.join("general.rel").is_file());

    let original_id = location.id;
    location.id = Uuid::new_v4();
    save_state(&location_path(original.root()), &location, true).unwrap();
    assert!(purge_profile_data(original.root(), "original").is_err());
    assert!(directory.join("general.rel").is_file());

    location.id = original_id;
    save_state(&location_path(original.root()), &location, true).unwrap();
    purge_profile_data(original.root(), "original").unwrap();
    assert!(!directory.exists());
}

fn mock_ego_chat(count: usize) -> (hermes_engine::Config, std::thread::JoinHandle<Vec<Value>>) {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
    };
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}/v1", socket.local_addr().unwrap());
    let requests = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            let (mut tcp, _) = socket.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut reader = BufReader::new(tcp.try_clone().unwrap());
            let mut size = None;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    size = value.trim().parse::<usize>().ok();
                }
            }
            let mut body = vec![0; size.unwrap()];
            reader.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice::<Value>(&body).unwrap());
            let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Fixture reply\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            write!(tcp, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}", sse.len()).unwrap();
        }
        requests
    });

    let config = hermes_engine::Config {
        base_url: origin,
        model: "fixture".into(),
        api_key: String::new(),
    };
    (config, requests)
}

#[tokio::test]
async fn native_request_renders_approved_profile_identity_and_pins_once() {
    use hermes_engine::{
        CancellationToken, Session, Status,
        native_ego::{PinPriority, PinRequest, PinScope},
        settings::ProfileSettings,
    };
    use hermes_library::memory_plugin::{MemorySources, MemoryStore};
    use std::sync::Arc;

    struct Sources(CaptureBinding);
    impl MemorySources for Sources {
        fn check_session(&self, _: &str, binding: &CaptureBinding) -> Result<()> {
            ensure!(*binding == self.0, "wrong fixture session");
            Ok(())
        }
        fn copied_source_change_pending(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let (_, home, general, _, phy) = legacy_profile(temp.path());
    let library = Library::open(home.clone()).unwrap();
    let options = json!({
        "memory_provider":"reliquary","memory_ego_enabled":true,"timezone":"UTC",
        "mcp_tools":false,"clock_tools":false,"terminal_tools":false,"file_tools":false,"skill_tools":false,
    });
    ProfileSettings::update(&home, options.as_object().unwrap()).unwrap();
    fs::write(home.join("SOUL.md"), "Mercury fallback persona.").unwrap();
    let notes = home.join("memories");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("MEMORY.md"), "Legacy Mercury memory sentinel.").unwrap();
    fs::write(notes.join("USER.md"), "Legacy Mercury user sentinel.").unwrap();

    let memory = Arc::new(open(&library, "work", false).unwrap().unwrap());
    let binding = CaptureBinding {
        profile_id: "work".into(),
        principal_id: "legacy-principal".into(),
        project_id: None,
        rel_id: general.to_string(),
        phy_id: phy.to_string(),
    };
    let ego = memory
        .ego(
            Arc::new(Sources(binding.clone())),
            "fixture".into(),
            Some(binding),
        )
        .unwrap()
        .unwrap();
    ego.pin(PinRequest::PinAnchor {
        scope: PinScope::Phy,
        text: "Profile pin alpha sentinel.".into(),
        priority: None,
    })
    .unwrap();
    let pin = ego
        .pin(PinRequest::PinAnchor {
            scope: PinScope::Rel,
            text: "Project pin beta sentinel.".into(),
            priority: Some(PinPriority::Low),
        })
        .unwrap();
    let pin = &pin["pin"];
    let raised = ego
        .pin(PinRequest::ModifyAnchor {
            scope: PinScope::Rel,
            id: pin["id"].as_str().unwrap().into(),
            revision: pin["revision"].as_u64().unwrap(),
            text: None,
            priority: Some(PinPriority::Raise),
        })
        .unwrap();
    assert_eq!(raised["pin"]["text"], "Project pin beta sentinel.");
    assert_eq!(raised["pin"]["priority"], "normal");

    let draft = "Exact approved Mercury persona.\nIts second line remains verbatim.";
    let pending = ego.prepare_identity(draft.into()).unwrap();
    assert_eq!(pending.review["text"], draft);
    assert!(ego.identity().unwrap().is_none());

    let (config, requests) = mock_ego_chat(2);
    let mut session =
        Session::open_for_profile_with_enrollment(&temp.path().join("chat"), None, &home, true)
            .unwrap();
    let (events, _received) = async_channel::unbounded();
    session.set_native_ego(Some(ego.clone()));
    session
        .turn(
            config.clone(),
            "Before approval.".into(),
            events.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        session.history().turns.last().unwrap().status,
        Status::Complete
    );
    (pending.apply)().unwrap();
    assert_eq!(ego.identity().unwrap().as_deref(), Some(draft));
    session.set_native_ego(Some(ego));
    session
        .turn(
            config,
            "After approval.".into(),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        session.history().turns.last().unwrap().status,
        Status::Complete
    );

    let requests = requests.join().unwrap();
    let prompt = |index: usize| requests[index]["messages"][0]["content"].as_str().unwrap();
    assert!(prompt(0).contains("Mercury fallback persona."));
    assert!(!prompt(0).contains(draft));
    assert!(!prompt(1).contains("Mercury fallback persona."));
    assert_eq!(prompt(1).matches(draft).count(), 1);
    assert!(prompt(1).find(draft).unwrap() < prompt(1).find("User timezone: UTC.").unwrap());
    assert!(prompt(1).contains("You are Hermes, a helpful desktop agent."));
    for marker in ["Profile pin alpha sentinel.", "Project pin beta sentinel."] {
        assert_eq!(prompt(1).matches(marker).count(), 1);
    }
    for index in [0, 1] {
        assert!(!prompt(index).contains("Legacy Mercury memory sentinel."));
        assert!(!prompt(index).contains("Legacy Mercury user sentinel."));
    }
    let current = requests[1]["messages"].as_array().unwrap();
    assert_eq!(current[1]["content"], "Before approval.");
    assert_eq!(current[2]["content"], "Fixture reply");
    assert_eq!(current[3]["content"], "After approval.");
    for name in ["list_pins", "pin_anchor", "modify_anchor", "delete_anchor"] {
        assert_eq!(
            requests[1]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["function"]["name"] == name)
                .count(),
            1
        );
    }
}
