//! Metadata-only conversation catalog. No provider is launched or resumed to inspect it.
use crate::model::{Agent, Store, home};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub provider: Agent,
    pub provider_home: PathBuf,
    pub cwd: PathBuf,
    pub name: String,
    pub updated: u64,
    pub parent: Option<Uuid>,
    pub subagent: bool,
    pub archived: bool,
    pub source: String,
    pub managed_item: Option<Uuid>,
    pub transcript_path: Option<PathBuf>,
    pub provider_session: Option<String>,
}
pub fn metadata(path: &Path, root: &Path, archived: bool) -> Option<Conversation> {
    let mut bytes = vec![];
    BufReader::new(File::open(path).ok()?)
        .take(65536)
        .read_until(b'\n', &mut bytes)
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    if v["type"] != "session_meta" {
        return None;
    }
    let p = &v["payload"];
    let cwd = PathBuf::from(p["cwd"].as_str()?);
    if !cwd.is_absolute() {
        return None;
    };
    Some(Conversation {
        id: Uuid::parse_str(p["id"].as_str()?).ok()?,
        provider: Agent::Codex,
        provider_home: root.to_path_buf(),
        name: cwd
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        cwd,
        updated: path
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
        parent: p["parent_thread_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok()),
        subagent: p["source"].get("subagent").is_some(),
        archived,
        source: "bounded_session_metadata".into(),
        managed_item: None,
        transcript_path: Some(path.to_path_buf()),
        provider_session: None,
    })
}
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, errors: &mut Vec<String>) {
    if depth > 6 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(v) => v,
        Err(e) => {
            if dir.exists() {
                errors.push(format!("{}: {e}", dir.display()))
            }
            return;
        }
    };
    for entry in entries {
        match entry {
            Ok(e) => {
                let Ok(kind) = e.file_type() else { continue };
                if kind.is_dir() {
                    walk(&e.path(), depth + 1, out, errors)
                } else if kind.is_file() && e.path().extension().is_some_and(|s| s == "jsonl") {
                    out.push(e.path())
                }
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
}
pub fn collect(store: &Store) -> Result<(Vec<Conversation>, Vec<String>)> {
    type MetadataCache = BTreeMap<PathBuf, (u64, std::time::SystemTime, Option<Conversation>)>;
    static CACHE: OnceLock<Mutex<MetadataCache>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut seen = std::collections::BTreeSet::new();
    let state = store.read()?;
    let default = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".codex"));
    let mut roots = vec![default];
    for e in &state.entries {
        if e.agent == Agent::Codex
            && let Some(root) = &e.provider_home
            && !roots.contains(root)
        {
            roots.push(root.clone())
        }
    }
    let mut conversations = BTreeMap::new();
    let mut errors = vec![];
    for root in roots {
        for (name, archived) in [("sessions", false), ("archived_sessions", true)] {
            let mut paths = vec![];
            walk(&root.join(name), 0, &mut paths, &mut errors);
            for path in paths {
                seen.insert(path.clone());
                let signature = path
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok().map(|time| (m.len(), time)));
                let parsed = if let Some((len, time)) = signature {
                    if let Some((old_len, old_time, value)) =
                        cache.get(&path).filter(|(l, t, _)| *l == len && *t == time)
                    {
                        let _ = (old_len, old_time);
                        value.clone()
                    } else {
                        let value = metadata(&path, &root, archived);
                        cache.insert(path.clone(), (len, time, value.clone()));
                        value
                    }
                } else {
                    None
                };
                if let Some(mut c) = parsed {
                    c.provider_home = root.clone();
                    c.archived = archived;
                    c.managed_item = state
                        .entries
                        .iter()
                        .find(|e| {
                            e.agent == Agent::Codex
                                && e.session_verified
                                && e.session_id == Some(c.id)
                                && e.provider_home.as_ref().unwrap_or(&root) == &root
                        })
                        .map(|e| e.id);
                    conversations.insert((root.clone(), c.id), c);
                }
            }
        }
    }
    cache.retain(|path, _| seen.contains(path));
    for e in state
        .entries
        .iter()
        .filter(|e| matches!(e.agent, Agent::Claude | Agent::Cursor) && e.session_verified)
    {
        if let Some(id) = e
            .session_id
            .or_else(|| {
                e.provider_session
                    .as_ref()
                    .and_then(|s| Uuid::parse_str(s).ok())
            })
            .or_else(|| (e.agent == Agent::Cursor && e.provider_session.is_some()).then_some(e.id))
        {
            conversations.insert(
                (e.provider_home.clone().unwrap_or_default(), id),
                Conversation {
                    id,
                    provider: e.agent,
                    provider_home: e.provider_home.clone().unwrap_or(home()?.join(".claude")),
                    cwd: e.cwd.clone(),
                    name: e.name.clone(),
                    updated: 0,
                    parent: None,
                    subagent: false,
                    archived: false,
                    source: "managed_binding".into(),
                    managed_item: Some(e.id),
                    transcript_path: e.transcript_path.clone(),
                    provider_session: e.provider_session.clone(),
                },
            );
        }
    }
    let mut list: Vec<_> = conversations.into_values().collect();
    list.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.id.cmp(&b.id)));
    Ok((list, errors))
}
pub fn search(store: &Store, query: &str, limit: usize, cursor: usize) -> Result<Value> {
    search_with_backend(store, query, limit, cursor, false)
}
pub fn search_with_backend(
    store: &Store,
    query: &str,
    limit: usize,
    cursor: usize,
    semantic: bool,
) -> Result<Value> {
    let (list, mut errors) = collect(store)?;
    let descriptions: std::collections::BTreeMap<_, _> = store
        .documents::<crate::descriptions::SavedDescription>("descriptions")?
        .into_iter()
        .map(|d| (d.item, d))
        .collect();
    let mut matched: Vec<_> = list
        .iter()
        .filter(|c| !c.subagent)
        .filter_map(|c| {
            let description = c.managed_item.and_then(|id| descriptions.get(&id));
            crate::search::fuzzy(query, &crate::search::document(c, description))
                .map(|score| (score, c.clone()))
        })
        .collect();
    matched.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.updated.cmp(&a.1.updated)));
    let fuzzy_scores: BTreeMap<_, _> = matched
        .iter()
        .map(|(score, c)| ((c.provider_home.clone(), c.id), *score))
        .collect();
    let mut backend = "fuzzy";
    let mut model_result = None;
    if semantic && !query.trim().is_empty() {
        let pool: Vec<_> = matched.iter().take(25).map(|(_, c)| c.clone()).collect();
        let candidates: Vec<_> = pool
            .into_iter()
            .map(|c| {
                let description = c.managed_item.and_then(|id| descriptions.get(&id)).cloned();
                (c, description)
            })
            .collect();
        if !candidates.is_empty() {
            match crate::search::rerank(store, query, &candidates) {
                Ok(result) => {
                    let ranked: Vec<_> = result["ranked"]
                        .as_array()
                        .context("Semantic results missing")?
                        .iter()
                        .filter_map(|r| {
                            serde_json::from_value::<Conversation>(r["conversation"].clone()).ok()
                        })
                        .enumerate()
                        .map(|(i, c)| (i as i64, c))
                        .collect();
                    let mut combined = ranked;
                    combined.extend(matched.into_iter().skip(candidates.len()));
                    matched = combined;
                    backend = "jev";
                    model_result = Some(result);
                }
                Err(e) => errors.push(e.to_string()),
            }
        }
    }
    let total = matched.len();
    let limit = limit.clamp(1, 100);
    let page: Vec<_> = matched
        .into_iter()
        .skip(cursor)
        .take(limit)
        .map(|(fuzzy_score, c)| {
            let relevance = model_result
                .as_ref()
                .and_then(|r| r["ranked"].as_array())
                .and_then(|rows| {
                    rows.iter().find(|r| {
                        r["conversation"]["id"] == c.id.to_string()
                            && r["conversation"]["provider_home"]
                                == c.provider_home.to_string_lossy().as_ref()
                    })
                });
            let mut item = serde_json::to_value(&c).unwrap_or_default();
            item["fuzzy_score"] = json!(
                fuzzy_scores
                    .get(&(c.provider_home.clone(), c.id))
                    .copied()
                    .unwrap_or(fuzzy_score)
            );
            item["classification"] = relevance
                .cloned()
                .map(|mut r| {
                    r.as_object_mut().unwrap().remove("conversation");
                    r
                })
                .unwrap_or(Value::Null);
            item
        })
        .collect();
    let next = (cursor + page.len() < total).then_some(cursor + page.len());
    Ok(
        json!({"schema_version":1,"items":page,"total":total,"next_cursor":next,"backend":backend,"semantic":model_result,"errors":errors,"content":"metadata_and_cached_descriptions_only"}),
    )
}
pub fn manage(store: &Store, conversation: &Conversation) -> Result<Uuid> {
    if conversation.archived {
        anyhow::bail!("Archived conversation: unarchive using the provider before launching")
    };
    if let Some(id) = conversation.managed_item {
        return Ok(id);
    }
    let id = Uuid::new_v4();
    store.update(|s| {
        if let Some(e) = s.entries.iter().find(|e| {
            e.session_id == Some(conversation.id)
                && e.agent == conversation.provider
                && e.provider_home.as_ref() == Some(&conversation.provider_home)
        }) {
            return Ok(e.id);
        };
        s.entries.push(crate::model::Entry {
            id,
            name: conversation.name.clone(),
            workspace: "default".into(),
            cwd: conversation.cwd.clone(),
            agent: conversation.provider,
            session_id: Some(conversation.id),
            session_verified: true,
            provider_home: Some(conversation.provider_home.clone()),
            resume_args: Some(vec![]),
            ever_started: true,
            transcript_path: conversation.transcript_path.clone(),
            provider_session: conversation.provider_session.clone(),
            ..Default::default()
        });
        Ok(id)
    })
}
pub fn select(store: &Store, id: Uuid) -> Result<Conversation> {
    let mut list: Vec<_> = collect(store)?
        .0
        .into_iter()
        .filter(|c| c.id == id && !c.subagent)
        .collect();
    if list.len() != 1 {
        anyhow::bail!("Conversation ID is missing or ambiguous across provider homes")
    };
    list.pop().context("Conversation missing")
}
