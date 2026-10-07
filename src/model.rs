use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, clap::ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Claude,
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
                Self::Shell => "shell",
                Self::Command => "command",
            }
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub workspace: String,
    pub name: String,
    pub cwd: PathBuf,
    pub agent: Agent,
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub isolated: bool,
    #[serde(default)]
    pub window: usize,
    #[serde(default)]
    pub terminal_id: Option<String>,
    #[serde(default)]
    pub imported: bool,
    #[serde(default)]
    pub ever_started: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub token: Uuid,
    pub pid: u32,
    pub start_time: u64,
    pub terminal_id: Option<String>,
    pub ended: bool,
    pub hooks_seen: bool,
    pub agents: BTreeMap<String, String>,
    pub agents_started: usize,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
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
            .context("No saved tab with that ID")
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("Unsupported workspace format {}", self.version);
        }
        for e in &self.entries {
            if !e.cwd.is_absolute() {
                bail!("Working directory must be absolute: {}", e.cwd.display());
            }
            if e.name.chars().any(char::is_control) {
                bail!("Tab names cannot contain control characters");
            }
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Store {
    pub dir: PathBuf,
}
impl Store {
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = dir
            .or_else(|| std::env::var_os("GWS_STATE_DIR").map(PathBuf::from))
            .unwrap_or(home()?.join(".local/share/ghostty-workspaces"));
        let dir = if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir()?.join(dir)
        };
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { dir })
    }
    pub fn read(&self) -> Result<State> {
        let path = self.dir.join("workspaces.json");
        if !path.exists() {
            return Ok(State::default());
        }
        let state: State = serde_json::from_slice(&fs::read(&path)?)
            .context("Cannot read workspaces.json; leaving it untouched")?;
        state.validate()?;
        Ok(state)
    }
    pub fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("state.lock"))?;
        lock.lock_exclusive()?;
        let mut state = self.read()?;
        let result = f(&mut state)?;
        state.validate()?;
        atomic_write(
            &self.dir.join("workspaces.json"),
            &serde_json::to_vec_pretty(&state)?,
        )?;
        Ok(result)
    }
}
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let mut file = File::create(&temp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
