use super::*;
use crate::{memory::save_state, storage::legacy};
use axum::{Json, Router, routing::post};
use hermes_engine::{credentials::EndpointStore, settings::ProfileSettings};
use hermes_library::{Library, memory_plugin::MemoryStore};
use reliquary_memory::{ObservationSource, ObservationStanding, OwnerKind, Phylactery, Rel};
use std::{fs, path::PathBuf};

struct NativeChat {
    id: String,
    binding: CaptureBinding,
}

impl MemorySources for NativeChat {
    fn check_session(&self, id: &str, binding: &CaptureBinding) -> Result<()> {
        ensure!(
            id == self.id && binding == &self.binding,
            "unexpected fixture chat"
        );
        Ok(())
    }

    fn copied_source_change_pending(&self, _: &str) -> Result<bool> {
        Ok(false)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires disposable Core mixed-recall fixture files"]
async fn native_query_returns_one_owner_qualified_ranking_and_fences_a_phy_switch() {
    let fixture = PathBuf::from(
        std::env::var_os("MERCURY_PHY_PERCEPTION_FIXTURE")
            .expect("set MERCURY_PHY_PERCEPTION_FIXTURE to the disposable fixture folder"),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let decision = |Json(request): Json<Value>| async move {
            let packet = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .filter_map(|message| serde_json::from_str::<Value>(message).ok())
                .find(|packet| packet["state"]["operation"] == "memory_query")
                .expect("scoped native memory decision");
            let approved = packet["state"]["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["memory"].clone())
                .collect::<Vec<_>>();
            Json(
                json!({"choices":[{"index":0,"message":{"role":"assistant","content":
                json!({"version":1,"approved":approved}).to_string()},"finish_reason":"stop"}]}),
            )
        };
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", post(decision)),
        )
        .await
        .unwrap();
    });
    let cases = tokio::task::spawn_blocking(move || -> Result<()> {
        let config = Config {
            base_url: format!("http://{address}/v1"),
            model: "fixture".into(),
            api_key: String::new(),
        };
        for (filename, current_claim) in [("current.phy", true), ("pending.phy", false)] {
            let temp = tempfile::tempdir()?;
            let home = temp.path();
            let store = home.join("reliquary");
            fs::create_dir(&store)?;
            let rel_path = store.join("mixed.rel");
            let private_path = store.join("private.phy");
            fs::copy(fixture.join("mixed.rel"), &rel_path)?;
            fs::copy(fixture.join(filename), &private_path)?;
            let rel = Rel::open(rel_path)?;
            let private = Phylactery::open(private_path)?;
            let switched = Phylactery::create(store.join("switched.phy"))?;
            let mut topology = json!({
                "version":2, "profile_id":"default", "principal_id":"fixture",
                "main_rel":rel.owner_id(), "active_phy":private.owner_id(), "projects":{},
                "owners":{
                    rel.owner_id().to_string():{"kind":"rel","path":rel.path()},
                    private.owner_id().to_string():{"kind":"phylactery","path":private.path()},
                    switched.owner_id().to_string():{"kind":"phylactery","path":switched.path()},
                }
            });
            let topology_path = store.join("topology-v1.json");
            save_state(&topology_path, &topology, false)?;
            save_state(
                &store.join("capture-enrollment-v1.json"),
                &json!({"version":1,"cutoff":1}),
                false,
            )?;
            let embedding = EndpointStore::open(&home.join("providers"))?.save(
                Some("embedding"),
                "Fixture embedding",
                &config,
            )?;
            let settings = json!({"memory_embedding":{"endpoint_id":embedding.id,"dimensions":8}});
            ProfileSettings::update(home, settings.as_object().unwrap())?;

            let memory = Arc::new(legacy(home, "default")?.context("fixture not mounted")?);
            let library = Library::open(home.join("library"))?;
            let chat = library.create_session(None, Some("Native mixed recall"))?;
            let binding = memory
                .binding_for_new_session(&chat)?
                .context("fixture chat unbound")?;
            let (_, instance) = memory.route(&binding)?;
            let inspected =
                instance.list_observations(private.owner_id(), OwnerKind::Phylactery, None, 2)?;
            let [head] = inspected.items.as_slice() else {
                anyhow::bail!("expected the authorized private Observation head");
            };
            assert_eq!(head.claim.proposition, "Zephyr delivery was approved.");
            assert_eq!(head.standing == ObservationStanding::Current, current_claim);
            let ObservationSource::Observation(id) = head.claim.citation.source else {
                anyhow::bail!("missing exact private Observation citation");
            };
            let history = instance.observation_history(
                private.owner_id(),
                OwnerKind::Phylactery,
                id,
                None,
                2,
            )?;
            assert_eq!(history.items[0].claim.citation, head.claim.citation);
            let invoke = |admission: &str| {
                let sources = Arc::new(NativeChat {
                    id: chat.id.clone(),
                    binding: binding.clone(),
                });
                memory
                    .clone()
                    .query(
                        sources,
                        binding.clone(),
                        MemoryTurn {
                            session_id: chat.id.clone(),
                            admission_id: admission.into(),
                            admitted_ns: 1,
                            calendar_zone: "UTC".into(),
                        },
                    )
                    .invoke(
                        MemoryRequest::Query {
                            query: "needle zephyr".into(),
                        },
                        &config,
                    )
            };
            let MemorySelection::Result {
                count,
                content,
                check_disclosure,
            } = invoke("mixed")?
            else {
                anyhow::bail!("native query should return a typed selection");
            };
            assert_eq!(count, if current_claim { 3 } else { 2 });
            let memories = content["memories"].as_array().unwrap();
            assert_eq!(memories.len(), 2);
            let owners = memories
                .iter()
                .map(|row| row["reference"]["owner_id"].as_str().unwrap())
                .collect::<BTreeSet<_>>();
            let rel_id = rel.owner_id().to_string();
            let phy_id = private.owner_id().to_string();
            assert_eq!(owners, BTreeSet::from([rel_id.as_str(), phy_id.as_str()]));
            assert_eq!(
                memories[0]["reference"]["memory_id"],
                memories[1]["reference"]["memory_id"]
            );
            assert_ne!(memories[0]["id"], memories[1]["id"]);
            assert_ne!(
                memories[0]["reference"]["body_id"],
                memories[1]["reference"]["body_id"]
            );
            assert_eq!(content["ranked"].as_array().unwrap().len(), count);
            if current_claim {
                assert_eq!(
                    content["observations"][0]["content"],
                    "Zephyr delivery was approved."
                );
                assert_eq!(
                    content["observations"][0]["citation"]["owner_id"],
                    json!(private.owner_id().as_bytes())
                );
            } else {
                assert!(content.get("observations").is_none());
            }
            check_disclosure()?;

            let MemorySelection::Result {
                check_disclosure, ..
            } = invoke("switch")?
            else {
                anyhow::bail!("native query should prepare a selection before switching");
            };
            topology["active_phy"] = json!(switched.owner_id());
            save_state(&topology_path, &topology, true)?;
            assert!(check_disclosure().is_err());
            assert!(memory.needs_profile_remount()?);
            memory.pause_for_remount();
        }
        Ok(())
    })
    .await;
    server.abort();
    cases.unwrap().unwrap();
}
