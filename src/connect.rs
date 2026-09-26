//! Register the agent wallet as an MCP server in the agent clients we use.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    Claude,
    Codex,
    Gemini,
}

impl std::str::FromStr for Client {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            other => Err(anyhow::anyhow!(
                "unknown client '{other}'; use claude, codex or gemini"
            )),
        }
    }
}

/// The command an MCP client should launch.
pub fn server_command(wallet: &str) -> anyhow::Result<(String, Vec<String>)> {
    let binary = std::env::current_exe()?.canonicalize()?;
    Ok((
        binary.display().to_string(),
        vec!["--name".into(), wallet.into(), "agent".into(), "mcp".into()],
    ))
}

pub fn connect(client: Client, wallet: &str) -> anyhow::Result<String> {
    let (command, args) = server_command(wallet)?;
    match client {
        Client::Claude => connect_claude(&command, &args),
        Client::Codex => {
            let path = home_file(".codex/config.toml")?;
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let updated = upsert_codex(&existing, &command, &args);
            write_with_backup(&path, &existing, &updated)?;
            Ok(format!("Added [mcp_servers.manus] to {}", path.display()))
        }
        Client::Gemini => {
            let path = home_file(".gemini/settings.json")?;
            let existing = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
            let updated = upsert_gemini(&existing, &command, &args)?;
            write_with_backup(&path, &existing, &updated)?;
            Ok(format!("Added mcpServers.manus to {}", path.display()))
        }
    }
}

fn connect_claude(command: &str, args: &[String]) -> anyhow::Result<String> {
    // Replace any earlier user-scope registration, e.g. the legacy SSE daemon.
    let _ = Command::new("claude")
        .args(["mcp", "remove", "--scope", "user", "manus"])
        .output();
    let output = Command::new("claude")
        .args(["mcp", "add", "--scope", "user", "manus", "--", command])
        .args(args)
        .output()
        .map_err(|_| {
            anyhow::anyhow!(
                "The `claude` CLI was not found. Add this server manually:\n  claude mcp add --scope user manus -- {command} {}",
                args.join(" ")
            )
        })?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "claude mcp add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(
        "Registered `manus` for Claude Code in user scope. If a project still has an old \
        `manus` entry, run `claude mcp remove manus` inside that project."
            .into(),
    )
}

fn home_file(relative: &str) -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(relative))
}

fn write_with_backup(path: &Path, previous: &str, updated: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !previous.is_empty() && previous != updated {
        std::fs::write(path.with_extension("manus-backup"), previous)?;
    }
    std::fs::write(path, updated)?;
    Ok(())
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

const CODEX_MARKER: &str = "# Written by `manus agent connect codex`.";

/// Replace or append the `[mcp_servers.manus]` table, leaving the rest untouched.
fn upsert_codex(existing: &str, command: &str, args: &[String]) -> String {
    let mut output = Vec::new();
    let mut skipping = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            skipping =
                trimmed == "[mcp_servers.manus]" || trimmed.starts_with("[mcp_servers.manus.");
        }
        if !skipping && trimmed != CODEX_MARKER {
            output.push(line.to_string());
        }
    }
    while output.last().is_some_and(|line| line.trim().is_empty()) {
        output.pop();
    }
    let args = args
        .iter()
        .map(|a| toml_string(a))
        .collect::<Vec<_>>()
        .join(", ");
    output.push(String::new());
    output.push(CODEX_MARKER.into());
    output.push("[mcp_servers.manus]".into());
    output.push(format!("command = {}", toml_string(command)));
    output.push(format!("args = [{args}]"));
    output.join("\n") + "\n"
}

fn upsert_gemini(existing: &str, command: &str, args: &[String]) -> anyhow::Result<String> {
    let mut settings: serde_json::Value = serde_json::from_str(existing)
        .map_err(|e| anyhow::anyhow!("~/.gemini/settings.json is not valid JSON: {e}"))?;
    let servers = settings
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Gemini settings must be a JSON object"))?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    servers["manus"] = serde_json::json!({ "command": command, "args": args });
    Ok(serde_json::to_string_pretty(&settings)? + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_table_is_replaced_not_duplicated() {
        let existing = "model = \"x\"\n\n[mcp_servers.gbrain]\nurl = \"u\"\n\n[mcp_servers.manus]\ncommand = \"old\"\nargs = []\n\n[projects.\"/a\"]\ntrust_level = \"trusted\"\n";
        let args = vec!["agent".to_string(), "mcp".to_string()];
        let once = upsert_codex(existing, "/bin/manus", &args);
        let twice = upsert_codex(&once, "/bin/manus", &args);
        assert_eq!(once, twice);
        assert_eq!(twice.matches("[mcp_servers.manus]").count(), 1);
        assert!(twice.contains("[mcp_servers.gbrain]"));
        assert!(twice.contains("trust_level = \"trusted\""));
        assert!(!twice.contains("\"old\""));
    }

    #[test]
    fn gemini_keeps_other_servers() {
        let existing = r#"{"mcpServers":{"vbrl-brain":{"command":"node"}},"theme":"dark"}"#;
        let updated = upsert_gemini(existing, "/bin/manus", &["agent".into()]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(value["mcpServers"]["vbrl-brain"]["command"], "node");
        assert_eq!(value["mcpServers"]["manus"]["command"], "/bin/manus");
        assert_eq!(value["theme"], "dark");
    }
}
