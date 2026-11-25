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
