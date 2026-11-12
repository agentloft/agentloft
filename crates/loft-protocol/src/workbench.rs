//! Workbench wire types: file system, diffs, terminals and client calls.

use crate::ids::{ProjectId, TerminalId, ThreadId, TurnId};
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Raw bytes, base64-encoded on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bytes(pub Vec<u8>);

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

/// Where a relative path is resolved. Paths can never escape the root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum FsRoot {
    Project(ProjectId),
    /// The thread's working directory (repo root or its worktree).
    Thread(ThreadId),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirEntry {
    pub name: String,
    /// Relative to the root, `/`-separated.
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileContent {
    pub rel_path: String,
    pub abs_path: String,
    pub size: u64,
    /// Heuristic: contains NUL bytes in the first 8 KiB.
    pub binary: bool,
    /// The file goes on after `bytes`: one answer holds only so much. Ask
    /// again from `offset + bytes.len()` for the rest.
    pub truncated: bool,
    /// Where in the file `bytes` starts. A daemon from before reads could
    /// start anywhere always answers from 0, and says so here.
    #[serde(default, skip_serializing_if = "crate::is_zero")]
    pub offset: u64,
    pub bytes: Bytes,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "turn", rename_all = "snake_case")]
pub enum DiffScope {
    /// Uncommitted changes in the thread's working tree vs `HEAD`.
    WorkingTree,
    /// Changes made during the last finished turn.
    LastTurn,
    /// Everything since the thread's first turn started.
    Thread,
    /// What one turn changed (from its checkpoints).
    Turn(TurnId),
    /// Everything not on the remote yet: from where the branch left its
    /// upstream (else origin's default branch) to the working tree.
    Origin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffResult {
    pub scope: DiffScope,
    /// Unified diff text (`git diff` format).
    pub patch: String,
    pub files: Vec<DiffFileStat>,
    /// Why there is no diff (not a git repo, no checkpoints yet, …).
    pub note: Option<String>,
    /// What it was compared with, when that is a branch (`origin/main`).
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiffFileStat {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TerminalInfo {
    pub id: TerminalId,
    /// The reattach key it was opened with, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: Option<i32>,
}

/// How much of a thread `ReadThread` returns.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadDetail {
    /// User requests and final answers only.
    #[default]
    Summary,
    /// Plus tool calls (titles and short output).
    Full,
}

/// Calls the daemon makes into a capable client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientCall {
    /// A `loft_browser::BrowserCommand` as JSON, for the thread's browser.
    Browser {
        thread_id: ThreadId,
        command: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_trip_as_base64() {
        let b = Bytes(vec![0, 159, 146, 150, b'h']);
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(json, "\"AJ+Slmg=\"");
        assert_eq!(serde_json::from_str::<Bytes>(&json).unwrap(), b);
    }

    #[test]
    fn diff_scope_shape() {
        assert_eq!(
            serde_json::to_string(&DiffScope::LastTurn).unwrap(),
            r#"{"kind":"last_turn"}"#
        );
        assert_eq!(
            serde_json::to_string(&DiffScope::Turn(TurnId("t1".into()))).unwrap(),
            r#"{"kind":"turn","turn":"t1"}"#
        );
    }
}

/// A model a harness offers (Codex `model/list`, Claude's `initialize`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// What to pass as the thread's model.
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    /// The concrete model an alias stands for (`opus` → `claude-opus-5-5`),
    /// so a thread reporting either matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_id: Option<String>,
    /// Reasoning efforts the model accepts, lowest first; empty = none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// Whether a harness's CLI is installed (Settings → Agents, onboarding).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessStatus {
    pub installed: bool,
    #[serde(default)]
    pub path: Option<String>,
    /// `--version` output, first line.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// An image uploaded with a message (stored by the daemon).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub name: String,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub mime: String,
    pub data: Bytes,
}
