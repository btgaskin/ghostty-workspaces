//! Durable work registry. Reads never initialize or repair storage.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, clap::ValueEnum, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Claude,
    Cursor,
    #[default]
    Shell,
    Command,
}
impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Codex => "codex",
                Self::Claude => "claude",
                Self::Cursor => "cursor",
                Self::Shell => "shell",
                Self::Command => "command",
            }
        )
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    Saved,
    Active,
    Parked,
    Finished,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Lifetime {
    Run,
    #[default]
    Work,
    Persistent,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Readiness {
    #[serde(default)]
    pub argv: Vec<String>,
    pub socket: Option<String>,
    #[serde(default = "ready_timeout")]
    pub timeout_seconds: u64,
}
fn ready_timeout() -> u64 {
    30
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Service {
    pub owner: Uuid,
    #[serde(default)]
    pub lifetime: Lifetime,
    #[serde(default)]
    pub stop_argv: Vec<String>,
    pub readiness: Option<Readiness>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub operation: Uuid,
    pub pid: u32,
    pub start: u64,
}
impl Lease {
    pub fn live(&self) -> bool {
        crate::process::start_time(self.pid) == Some(self.start)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Entry {
    pub id: Uuid,
    pub workspace: String,
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub session_id: Option<Uuid>,
    pub session_verified: bool,
    pub args: Vec<String>,
    pub command: Vec<String>,
    pub isolated: bool,
    pub window: usize,
    pub terminal_id: Option<String>,
    pub imported: bool,
    pub ever_started: bool,
    pub revision: u64,
    pub intent: Intent,
    pub provider_home: Option<PathBuf>,
    pub resume_args: Option<Vec<String>>,
    pub service: Option<Service>,
    pub dependencies: Vec<Uuid>,
    pub lease: Option<Lease>,
    pub provider_session: Option<String>,
    pub transcript_path: Option<PathBuf>,
}
impl Entry {
    pub fn needs_session(&self) -> bool {
        matches!(self.agent, Agent::Codex | Agent::Claude | Agent::Cursor)
            && (self.imported || self.ever_started)
            && ((self.session_id.is_none() && self.provider_session.is_none())
                || !self.session_verified)
    }
    pub fn binding_key(&self) -> serde_json::Value {
        serde_json::json!([
            self.cwd,
            self.agent,
            self.session_id,
            self.session_verified,
            self.args,
            self.command,
            self.isolated,
            self.provider_home,
            self.resume_args,
            self.service,
            self.dependencies,
            self.provider_session
        ])
    }
    pub fn operational_key(&self) -> serde_json::Value {
        serde_json::json!([
            self.cwd,
            self.agent,
            self.session_id,
            self.session_verified,
            self.args,
            self.command,
            self.isolated,
            self.provider_home,
            self.resume_args,
            self.intent,
            self.service,
            self.dependencies,
            self.provider_session
        ])
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    pub token: Uuid,
    pub pid: u32,
    pub start_time: u64,
    pub terminal_id: Option<String>,
    pub ended: bool,
    pub hooks_seen: bool,
    pub agents: BTreeMap<String, String>,
    pub agents_started: usize,
    pub last_error: Option<String>,
    pub exit_code: Option<i32>,
    pub boot: String,
    pub consumed: bool,
    pub supervised: bool,
    pub child_pid: Option<u32>,
    pub child_start: Option<u64>,
    pub pgid: Option<i32>,
    pub created: u64,
    pub ended_at: Option<u64>,
    pub activity: String,
    pub activity_at: Option<u64>,
    pub activity_source: Option<String>,
    pub survivors: Vec<(u32, u64)>,
    pub adopted: bool,
    pub owner_run: Option<Uuid>,
}
impl Run {
    pub fn live(&self) -> bool {
        (self.boot.is_empty() || self.boot == boot_id())
            && (self
                .child_pid
                .zip(self.child_start)
                .is_some_and(|(pid, start)| crate::process::start_time(pid) == Some(start))
                || (!self.ended
                    && ((self.pid == 0 && now().saturating_sub(self.created) < 30)
                        || (self.pid != 0
                            && crate::process::start_time(self.pid) == Some(self.start_time)))))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub entries: Vec<Entry>,
    pub runs: BTreeMap<Uuid, Run>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            entries: vec![],
            runs: BTreeMap::new(),
        }
    }
}
impl State {
    pub fn entry(&self, id: Uuid) -> Result<&Entry> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .context("No saved work item with that ID")
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("Unsupported workspace format {}", self.version)
        }
        let mut ids = std::collections::HashSet::new();
        for e in &self.entries {
            if !ids.insert(e.id) {
                bail!("Duplicate work item ID")
            };
            if !e.cwd.is_absolute() {
                bail!("Working directory must be absolute: {}", e.cwd.display())
            };
            if e.name.chars().any(char::is_control) {
                bail!("Names cannot contain control characters")
            }
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Store {
    pub dir: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalRun {
    pub item: Uuid,
    pub run: Run,
}
impl Store {
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = dir
            .or_else(|| std::env::var_os("GWS_STATE_DIR").map(PathBuf::from))
            .unwrap_or(home()?.join(".local/share/ghostty-workspaces"));
        Ok(Self {
            dir: if dir.is_absolute() {
                dir
            } else {
                std::env::current_dir()?.join(dir)
            },
        })
    }
    pub fn database(&self) -> PathBuf {
        self.dir.join("gws.sqlite3")
    }
    fn legacy(&self) -> Result<State> {
        let path = self.dir.join("workspaces.json");
        if !path.exists() {
            return Ok(State::default());
        };
        let s: State = serde_json::from_slice(&fs::read(path)?)
            .context("Cannot read workspaces.json; leaving it untouched")?;
        s.validate()?;
        Ok(s)
    }
    fn check(c: &Connection) -> Result<()> {
        let version: u32 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 1 {
            bail!("Unsupported gws database format {version}")
        };
        Ok(())
    }
    fn empty_initialization(c: &Connection) -> Result<bool> {
        let version: u32 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let tables: u32 = c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        Ok(version == 0 && tables == 0)
    }
    fn check_migration(&self) -> Result<()> {
        let original = self.dir.join("workspaces.json");
        if original.exists() {
            let current: serde_json::Value = serde_json::from_slice(&fs::read(original)?)?;
            if current["version"] == 1 {
                let backup = self.dir.join("workspaces.v1.backup.json");
                let previous: serde_json::Value =
                    serde_json::from_slice(&fs::read(backup).context(
                        "Incomplete migration: missing original backup; state preserved",
                    )?)?;
                if current != previous {
                    bail!(
                        "Incomplete migration: legacy state changed after database import. Both copies preserved; reconcile before continuing"
                    )
                }
            } else if current["version"] != 2 || current["migrated_to"] != "gws.sqlite3" {
                bail!("Unsupported legacy state alongside database; original file preserved")
            }
        }
        Ok(())
    }
    fn load(c: &Connection) -> Result<State> {
        Self::check(c)?;
        let mut s = State::default();
        let mut q = c.prepare("SELECT body FROM items ORDER BY rowid")?;
        for r in q.query_map([], |r| r.get::<_, String>(0))? {
            s.entries.push(serde_json::from_str(&r?)?)
        }
        let mut q = c.prepare("SELECT item_id,body FROM runs")?;
        for r in q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, body) = r?;
            s.runs
                .insert(Uuid::parse_str(&id)?, serde_json::from_str(&body)?);
        }
        s.validate()?;
        Ok(s)
    }
    pub fn read(&self) -> Result<State> {
        if !self.database().exists() {
            return self.legacy();
        };
        let c = Connection::open_with_flags(self.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if Self::empty_initialization(&c)? {
            return self.legacy();
        }
        self.check_migration()?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        let tx = c.unchecked_transaction()?;
        let state = Self::load(&tx)?;
        tx.commit()?;
        Ok(state)
    }
    fn lock(&self) -> Result<File> {
        fs::create_dir_all(&self.dir)?;
        private_dir(&self.dir)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        private_file(&self.dir.join("state.lock"))?;
        f.lock_exclusive()?;
        Ok(f)
    }
    fn connect_locked(&self) -> Result<Connection> {
        let fresh = if self.database().exists() {
            let existing =
                Connection::open_with_flags(self.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            if Self::empty_initialization(&existing)? {
                true
            } else {
                Self::check(&existing)?;
                self.check_migration()?;
                false
            }
        } else {
            true
        };
        let legacy = if fresh { Some(self.legacy()?) } else { None };
        if fresh && self.dir.join("workspaces.json").exists() {
            atomic_write(
                &self.dir.join("workspaces.v1.backup.json"),
                &fs::read(self.dir.join("workspaces.json"))?,
            )?;
        }
        let mut c = Connection::open(self.database())?;
        private_file(&self.database())?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        if let Some(s) = legacy {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch("CREATE TABLE items(id TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE runs(item_id TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE run_history(token TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE documents(namespace TEXT NOT NULL,id TEXT NOT NULL,body TEXT NOT NULL,created INTEGER NOT NULL,PRIMARY KEY(namespace,id)); PRAGMA user_version=1;")?;
            Self::write_state(&tx, &s)?;
            tx.commit()?;
        }
        Self::check(&c)?;
        // Old binaries validate version before writing. Fence them after DB commit.
        if self.dir.join("workspaces.json").exists()
            && fs::read(self.dir.join("workspaces.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .is_none_or(|v| v["version"] != 2)
        {
            let marker = serde_json::json!({"version":2,"entries":[],"runs":{},"migrated_to":"gws.sqlite3","backup":"workspaces.v1.backup.json"});
            atomic_write(
                &self.dir.join("workspaces.json"),
                &serde_json::to_vec_pretty(&marker)?,
            )?;
        }
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "synchronous", "FULL")?;
        for name in ["gws.sqlite3-wal", "gws.sqlite3-shm"] {
            let p = self.dir.join(name);
            if p.exists() {
                private_file(&p)?
            }
        }
        Ok(c)
    }
    fn write_state(c: &Connection, s: &State) -> Result<()> {
        c.execute("DELETE FROM items", [])?;
        c.execute("DELETE FROM runs", [])?;
        for e in &s.entries {
            c.execute(
                "INSERT INTO items VALUES(?1,?2)",
                params![e.id.to_string(), serde_json::to_string(e)?],
            )?;
        }
        for (id, r) in &s.runs {
            let body = serde_json::to_string(r)?;
            c.execute(
                "INSERT INTO runs VALUES(?1,?2)",
                params![id.to_string(), body],
            )?;
            c.execute(
                "INSERT OR REPLACE INTO run_history VALUES(?1,?2)",
                params![
                    r.token.to_string(),
                    serde_json::to_string(&HistoricalRun {
                        item: *id,
                        run: r.clone()
                    })?
                ],
            )?;
        }
        Ok(())
    }
    pub fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _lock = self.lock()?;
        let mut c = self.connect_locked()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut s = Self::load(&tx)?;
        let original = s.clone();
        let result = f(&mut s)?;
        for e in &mut s.entries {
            if let Ok(old) = original.entry(e.id) {
                if e.operational_key() != old.operational_key() {
                    e.revision = old.revision + 1;
                }
            } else {
                e.revision = 1;
            }
        }
        s.validate()?;
        Self::write_state(&tx, &s)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn document<T: serde::de::DeserializeOwned>(
        &self,
        namespace: &str,
        id: &str,
    ) -> Result<Option<T>> {
        if !self.database().exists() {
            return Ok(None);
        };
        let c = Connection::open_with_flags(self.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::check(&c)?;
        use rusqlite::OptionalExtension;
        let body: Option<String> = c
            .query_row(
                "SELECT body FROM documents WHERE namespace=?1 AND id=?2",
                params![namespace, id],
                |r| r.get(0),
            )
            .optional()?;
        body.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn documents<T: serde::de::DeserializeOwned>(&self, namespace: &str) -> Result<Vec<T>> {
        if !self.database().exists() {
            return Ok(vec![]);
        };
        let c = Connection::open_with_flags(self.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::check(&c)?;
        let mut q =
            c.prepare("SELECT body FROM documents WHERE namespace=?1 ORDER BY created DESC,id")?;
        let mut out = vec![];
        for body in q.query_map([namespace], |r| r.get::<_, String>(0))? {
            out.push(serde_json::from_str(&body?)?)
        }
        Ok(out)
    }
    pub fn put<T: Serialize>(&self, namespace: &str, id: &str, value: &T) -> Result<()> {
        self.edit_document(namespace, id, |_| Ok(Some(serde_json::to_value(value)?)))
            .map(|_| ())
    }
    pub fn edit_document<T>(
        &self,
        namespace: &str,
        id: &str,
        f: impl FnOnce(Option<serde_json::Value>) -> Result<T>,
    ) -> Result<T>
    where
        T: Serialize,
    {
        let _lock = self.lock()?;
        let mut c = self.connect_locked()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        use rusqlite::OptionalExtension;
        let old: Option<String> = tx
            .query_row(
                "SELECT body FROM documents WHERE namespace=?1 AND id=?2",
                params![namespace, id],
                |r| r.get(0),
            )
            .optional()?;
        let value = f(old.map(|b| serde_json::from_str(&b)).transpose()?)?;
        tx.execute(
            "INSERT OR REPLACE INTO documents VALUES(?1,?2,?3,?4)",
            params![namespace, id, serde_json::to_string(&value)?, now() as i64],
        )?;
        // Bound disposable caches, never work/run history or active model jobs.
        if namespace == "search_cache" {
            tx.execute("DELETE FROM documents WHERE namespace='search_cache' AND (created<?1 OR id IN (SELECT id FROM documents WHERE namespace='search_cache' ORDER BY created DESC,id LIMIT -1 OFFSET 128))",[now().saturating_sub(600) as i64])?;
        } else if namespace == "audits" {
            tx.execute("DELETE FROM documents WHERE namespace='audits' AND id IN (SELECT id FROM documents WHERE namespace='audits' ORDER BY created DESC,id LIMIT -1 OFFSET 64)",[])?;
        } else if namespace == "model_jobs" {
            tx.execute("DELETE FROM documents WHERE namespace='model_jobs' AND json_extract(body,'$.ended')=1 AND id IN (SELECT id FROM documents WHERE namespace='model_jobs' AND json_extract(body,'$.ended')=1 ORDER BY created DESC,id LIMIT -1 OFFSET 128)",[])?;
        }
        tx.commit()?;
        Ok(value)
    }
    pub fn paused(&self) -> bool {
        self.document::<serde_json::Value>("settings", "monitoring")
            .ok()
            .flatten()
            .is_some_and(|v| v["paused"] == true)
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.put(
            "settings",
            "monitoring",
            &serde_json::json!({"paused":paused,"at":now()}),
        )
    }
    pub fn history(
        &self,
        item: Option<Uuid>,
        limit: usize,
        cursor: usize,
    ) -> Result<Vec<HistoricalRun>> {
        if !self.database().exists() {
            return Ok(vec![]);
        }
        let c = Connection::open_with_flags(self.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::check(&c)?;
        self.check_migration()?;
        let mut q=c.prepare("SELECT body FROM run_history WHERE (?1 IS NULL OR json_extract(body,'$.item')=?1) ORDER BY json_extract(body,'$.run.created') DESC,token LIMIT ?2 OFFSET ?3")?;
        let rows = q.query_map(
            params![
                item.map(|id| id.to_string()),
                limit.clamp(1, 100) as i64,
                cursor.min(i64::MAX as usize) as i64
            ],
            |r| r.get::<_, String>(0),
        )?;
        rows.map(|r| serde_json::from_str(&r?).map_err(Into::into))
            .collect()
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn boot_id() -> String {
    use std::sync::OnceLock;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.boottime"])
                .output();
            if let Ok(o) = output
                && o.status.success()
            {
                return String::from_utf8_lossy(&o.stdout).trim().to_owned();
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(s) = fs::read_to_string("/proc/sys/kernel/random/boot_id") {
                return s.trim().to_owned();
            }
        }
        "boot-identity-unavailable".into()
    })
    .clone()
}
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
pub fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    private_file(&temp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&temp, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?
    }
    Ok(())
}
