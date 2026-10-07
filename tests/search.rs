use ghostty_workspaces::{
    catalog::{self, Conversation},
    model::{Agent, Store},
    search::{self, RelevanceClassifier},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;
struct Fixture {
    calls: AtomicUsize,
    revision: &'static str,
}
impl RelevanceClassifier for Fixture {
    fn revision(&self) -> String {
        self.revision.into()
    }
    fn classify(&self, _: &Value) -> anyhow::Result<Value> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let answer = |score, confidence, p: [f64; 3]| json!({"type":"score","score":score,"confidence":confidence,"probabilities":{"0":p[0],"1":p[1],"2":p[2]}});
        Ok(
            json!({"model":"fixture-model-v1","answers":{"candidate_0":answer(1.1,0.2,[0.2,0.5,0.3]),"candidate_1":answer(1.9,0.9,[0.0,0.1,0.9]),"candidate_2":answer(0.,1.,[1.,0.,0.]),"candidate_3":answer(1.4,0.2,[0.1,0.4,0.5])}}),
        )
    }
}
fn candidate(
    n: usize,
) -> (
    Conversation,
    Option<ghostty_workspaces::descriptions::SavedDescription>,
) {
    (
        Conversation {
            id: Uuid::from_u128(n as u128 + 1),
            provider: Agent::Codex,
            provider_home: "/private/fixture".into(),
            cwd: "/private/projects/search".into(),
            name: format!("candidate {n}"),
            updated: 0,
            parent: None,
            subagent: false,
            archived: false,
            source: "fixture".into(),
            managed_item: None,
            transcript_path: None,
            provider_session: None,
        },
        None,
    )
}
#[test]
fn confidence_gates_order_and_cache_tracks_backend_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(Some(dir.path().into())).unwrap();
    let candidates: Vec<_> = (0..4).map(candidate).collect();
    let first = Fixture {
        calls: AtomicUsize::new(0),
        revision: "local/v1",
    };
    let result =
        search::rerank_with_classifier(&store, "search", &candidates, &first, "fixture", 0.6)
            .unwrap();
    let order: Vec<_> = result["ranked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fuzzy_rank"].as_u64().unwrap())
        .collect();
    assert_eq!(order, [1, 0, 3, 2]);
    assert_eq!(result["ranked"][1]["assessment"], "uncertain");
    assert_eq!(result["ranked"][0]["backend"], "classifier");
    search::rerank_with_classifier(&store, "search", &candidates, &first, "fixture", 0.6).unwrap();
    assert_eq!(first.calls.load(Ordering::Relaxed), 1);
    let second = Fixture {
        calls: AtomicUsize::new(0),
        revision: "local/v2",
    };
    let result =
        search::rerank_with_classifier(&store, "search", &candidates, &second, "fixture", 0.6)
            .unwrap();
    assert_eq!(result["backend_revision"], "local/v2");
    assert_eq!(second.calls.load(Ordering::Relaxed), 1);
    store.pause(true).unwrap();
    assert!(
        search::rerank_with_classifier(&store, "new query", &candidates, &second, "fixture", 0.6)
            .is_err()
    );
    assert_eq!(second.calls.load(Ordering::Relaxed), 1);
}
#[test]
fn request_contains_basenames_and_descriptions_not_private_paths() {
    let payload = search::request("search", &[candidate(0)]).to_string();
    assert!(!payload.contains("/private"));
    assert!(!payload.contains("transcript_path"));
    assert!(payload.contains("project"));
}
#[test]
fn catalog_accepts_only_bounded_root_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let path = dir.path().join("fixture.jsonl");
    std::fs::write(
        &path,
        format!(
            "{}\nSECRET TOOL OUTPUT",
            json!({"type":"session_meta","payload":{"id":id,"cwd":"/project","source":"cli"}})
        ),
    )
    .unwrap();
    let c = catalog::metadata(&path, dir.path(), false).unwrap();
    assert_eq!(c.id, id);
    assert!(!c.subagent);
    std::fs::write(&path, vec![b'x'; 65537]).unwrap();
    assert!(catalog::metadata(&path, dir.path(), false).is_none());
}
