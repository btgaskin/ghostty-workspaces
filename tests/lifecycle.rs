use ghostty_workspaces::{
    engine,
    model::{Agent, Entry, HistoricalRun, Run, State, Store},
    operations::{self, Action},
    process,
};
use uuid::Uuid;
fn fixture() -> (tempfile::TempDir, Store, Uuid) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(Some(dir.path().into())).unwrap();
    let id = Uuid::new_v4();
    store
        .update(|s| {
            s.entries.push(Entry {
                id,
                cwd: dir.path().into(),
                name: "fixture".into(),
                agent: Agent::Shell,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    (dir, store, id)
}
#[test]
fn read_only_inventory_does_not_initialize_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("absent");
    let store = Store::new(Some(root.clone())).unwrap();
    assert!(store.read().unwrap().entries.is_empty());
    assert!(!root.exists());
}
#[test]
fn interrupted_empty_database_recovers_legacy_without_losing_ids() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(Some(dir.path().into())).unwrap();
    let id = Uuid::new_v4();
    let legacy = State {
        entries: vec![Entry {
            id,
            name: "old".into(),
            cwd: dir.path().into(),
            agent: Agent::Shell,
            ..Default::default()
        }],
        ..Default::default()
    };
    std::fs::write(
        dir.path().join("workspaces.json"),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    rusqlite::Connection::open(store.database()).unwrap();
    assert_eq!(store.read().unwrap().entries[0].id, id);
    store.update(|_| Ok(())).unwrap();
    assert_eq!(store.read().unwrap().entries[0].id, id);
    assert!(dir.path().join("workspaces.v1.backup.json").is_file());
}
#[test]
fn post_import_legacy_changes_are_preserved_and_refused() {
    let (dir, store, id) = fixture();
    let mut original = store.read().unwrap();
    std::fs::write(
        dir.path().join("workspaces.v1.backup.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    original.entries[0].name = "legacy update after crash".into();
    std::fs::write(
        dir.path().join("workspaces.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    assert!(
        store
            .read()
            .unwrap_err()
            .to_string()
            .contains("legacy state changed")
    );
    assert!(
        store
            .update(|s| {
                s.entries.retain(|e| e.id != id);
                Ok(())
            })
            .is_err()
    );
    assert!(
        std::fs::read_to_string(dir.path().join("workspaces.json"))
            .unwrap()
            .contains("legacy update")
    );
}
#[test]
fn runs_are_retained_after_new_execution_and_forget() {
    let (_dir, store, id) = fixture();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    for (token, created) in [(first, 1), (second, 2)] {
        store
            .update(|s| {
                s.runs.insert(
                    id,
                    Run {
                        token,
                        created,
                        ended: true,
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
    }
    store
        .update(|s| {
            s.entries.clear();
            s.runs.clear();
            Ok(())
        })
        .unwrap();
    let rows: Vec<HistoricalRun> = store.history(Some(id), 1, 0).unwrap();
    assert_eq!(rows[0].run.token, second);
    assert_eq!(store.history(None, 10, 0).unwrap().len(), 2);
    assert_eq!(store.history(Some(id), 1, 1).unwrap()[0].run.token, first);
}
#[test]
fn accepted_operation_cannot_execute_after_expiry_or_boot_change() {
    for reboot in [false, true] {
        let (_dir, store, id) = fixture();
        let mut plan =
            operations::preview_human(&store, Action::Park, &[id], &[], None, false, None, None)
                .unwrap();
        operations::accept(&store, plan.id, false).unwrap();
        if reboot {
            plan.boot = "another-boot".into();
        } else {
            plan.expires_at = 0;
        }
        store.put("plans", &plan.id.to_string(), &plan).unwrap();
        let receipt = operations::apply(&store, plan.id, false).unwrap();
        assert_eq!(receipt.state, "refused");
        assert_eq!(
            store.read().unwrap().entries[0].intent,
            ghostty_workspaces::model::Intent::Saved
        );
        assert_eq!(
            operations::operation(&store, plan.id).unwrap().state,
            "refused"
        );
    }
}
#[test]
fn stale_preview_refuses_and_idempotent_apply_retains_receipt() {
    let (_dir, store, id) = fixture();
    let plan = operations::preview_human(&store, Action::Park, &[id], &[], None, false, None, None)
        .unwrap();
    store
        .update(|s| {
            s.entries[0].cwd = "/changed".into();
            Ok(())
        })
        .unwrap();
    let receipt = operations::apply(&store, plan.id, false).unwrap();
    assert_eq!(receipt.steps[0].state, "refused");
    assert_eq!(
        operations::apply(&store, plan.id, false).unwrap().steps[0].error,
        receipt.steps[0].error
    );
}
#[test]
fn automation_without_caller_cannot_park_and_human_preview_can() {
    let (_dir, store, id) = fixture();
    let agent =
        operations::preview(&store, Action::Park, &[id], &[], None, false, None, None).unwrap();
    assert_eq!(
        agent.targets[0].blocked.as_ref().unwrap().code,
        "caller_unresolved"
    );
    assert!(
        operations::preview_human(&store, Action::Park, &[id], &[], None, false, None, None)
            .unwrap()
            .targets[0]
            .blocked
            .is_none()
    );
}
#[test]
fn direct_launch_refuses_missing_or_reserved_dependency() {
    let (dir, store, id) = fixture();
    let dep = Uuid::new_v4();
    store
        .update(|s| {
            s.entries.push(Entry {
                id: dep,
                cwd: dir.path().into(),
                name: "dependency".into(),
                agent: Agent::Command,
                command: vec!["fixture".into()],
                ..Default::default()
            });
            s.entries[0].dependencies.push(dep);
            Ok(())
        })
        .unwrap();
    assert!(
        engine::authorize_launch(&store, id)
            .unwrap_err()
            .to_string()
            .contains("not active")
    );
    let pid = std::process::id();
    let start = process::start_time(pid).unwrap();
    store
        .update(|s| {
            s.runs.insert(
                dep,
                Run {
                    token: Uuid::new_v4(),
                    pid,
                    start_time: start,
                    ..Default::default()
                },
            );
            s.entries.iter_mut().find(|e| e.id == dep).unwrap().lease =
                Some(ghostty_workspaces::model::Lease {
                    operation: Uuid::new_v4(),
                    pid,
                    start,
                });
            Ok(())
        })
        .unwrap();
    assert!(
        engine::authorize_launch(&store, id)
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );
    assert!(!store.read().unwrap().runs.contains_key(&id));
}
#[test]
fn a_recorded_live_child_blocks_duplicate_even_with_inconsistent_end_flag() {
    let (_dir, store, id) = fixture();
    let pid = std::process::id();
    let start = process::start_time(pid).unwrap();
    store
        .update(|s| {
            s.runs.insert(
                id,
                Run {
                    token: Uuid::new_v4(),
                    ended: true,
                    child_pid: Some(pid),
                    child_start: Some(start),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
    assert!(store.read().unwrap().runs[&id].live());
    assert!(engine::authorize_launch(&store, id).is_err());
}
#[test]
fn profiling_refuses_new_model_jobs() {
    let (_dir, store, _) = fixture();
    store.pause(true).unwrap();
    assert!(ghostty_workspaces::util::ModelJobGuard::start(&store, "test").is_err());
    assert!(
        ghostty_workspaces::util::active_model_jobs(&store)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn future_legacy_schema_beside_database_is_never_overwritten() {
    let (dir, store, _) = fixture();
    let path = dir.path().join("workspaces.json");
    let bytes = b"{\"version\":3,\"future\":\"preserve this\"}";
    std::fs::write(&path, bytes).unwrap();
    assert!(store.read().is_err());
    assert!(store.update(|_| Ok(())).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
