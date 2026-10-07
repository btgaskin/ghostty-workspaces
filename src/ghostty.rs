use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub tabs: Vec<Tab>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub name: String,
    pub terminals: Vec<Surface>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub id: String,
    pub cwd: String,
}

pub fn jxa(source: &str) -> Result<String> {
    let mut child = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .context("Cannot open AppleScript stdin")?
        .write_all(source.as_bytes())?;
    let start = std::time::Instant::now();
    let stdout = child.stdout.take().context("Missing automation stdout")?;
    let stderr = child.stderr.take().context("Missing automation stderr")?;
    let drain = |stream: Box<dyn std::io::Read + Send>| {
        let mut bytes = vec![];
        let _ = stream.take(1024 * 1024).read_to_end(&mut bytes);
        bytes
    };
    let out = std::thread::spawn(move || drain(Box::new(stdout)));
    let err = std::thread::spawn(move || drain(Box::new(stderr)));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        };
        if start.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else {
        bail!("Ghostty automation timed out after 5 seconds")
    };
    let output = std::process::Output {
        status,
        stdout,
        stderr,
    };
    if !output.status.success() {
        bail!(
            "Ghostty automation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
pub fn snapshot() -> Result<Vec<Window>> {
    let text = jxa(r#"const a=Application('Ghostty');
JSON.stringify(a.windows().map(w=>({id:w.id(),tabs:w.tabs().map(t=>({id:t.id(),name:t.name(),terminals:t.terminals().map(p=>({id:p.id(),cwd:p.workingDirectory()}))}))})))"#)?;
    serde_json::from_str(&text).context("Ghostty returned an unexpected layout")
}
pub fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("string serialization")
}
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
pub fn open(
    cwd: &str,
    command: &str,
    title: &str,
    window: Option<&str>,
) -> Result<(String, String)> {
    let target = match window {
        Some(id) => format!("a.windows.whose({{id:{}}})()[0]", quote(id)),
        None => "null".into(),
    };
    let text = jxa(&format!(
        r#"const a=Application('Ghostty');
const cfg=a.newSurfaceConfiguration();
cfg.initialWorkingDirectory={cwd}; cfg.command={command}; cfg.waitAfterCommand=true;
let w={target}; let t;
if(w){{t=a.newTab({{in:w,withConfiguration:cfg}});}}
else{{w=a.newWindow({{withConfiguration:cfg}});t=w.selectedTab();}}
const p=t.focusedTerminal();
a.performAction('set_tab_title:'+{title},{{on:p}});
JSON.stringify({{window:w.id(),terminal:p.id()}})"#,
        cwd = quote(cwd),
        command = quote(command),
        title = quote(title)
    ))?;
    let value: serde_json::Value = serde_json::from_str(&text)?;
    Ok((
        value["window"]
            .as_str()
            .context("Missing window ID")?
            .into(),
        value["terminal"]
            .as_str()
            .context("Missing terminal ID")?
            .into(),
    ))
}
pub fn focus(id: &str) -> Result<()> {
    jxa(&format!(
        "const a=Application('Ghostty'); const p=a.terminals.whose({{id:{}}})(); if(!p.length) throw Error('Tab is no longer open'); a.focus(p[0]);",
        quote(id)
    ))?;
    Ok(())
}
pub fn close(id: &str) -> Result<()> {
    jxa(&format!(
        "const a=Application('Ghostty'); const p=a.terminals.whose({{id:{}}})(); if(!p.length) throw Error('Tab is no longer open'); a.close(p[0]);",
        quote(id)
    ))?;
    Ok(())
}
pub fn terminal_exists(windows: &[Window], id: &str) -> bool {
    windows
        .iter()
        .flat_map(|w| &w.tabs)
        .flat_map(|t| &t.terminals)
        .any(|s| s.id == id)
}
