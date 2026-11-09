//! Wire types shared by `agentloftd` and its clients (PLAN §1.6).
//!
//! Everything here is plain serde data: no I/O, no async, no GPUI. Frames are
//! length-delimited JSON (`u32` big-endian length prefix) carrying [`ClientMsg`]
//! and [`ServerMsg`].

pub mod ids;
pub mod model;

pub use ids::*;
pub use model::*;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible wire change. The daemon accepts `N` and `N-1`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum frame size accepted on the socket (16 MiB).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Client → daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello(Hello),
    Request { id: u64, body: Request },
    Subscribe { sub_id: u64, topic: Topic },
    Unsubscribe { sub_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol_version: u32,
    pub client_kind: ClientKind,
    pub client_build: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    LocalUi,
    RemoteUi,
    McpBridge,
    Cli,
    Test,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    AddProject {
        path: String,
        #[serde(default)]
        name: Option<String>,
    },
    CreateThread {
        project_id: ProjectId,
        harness: HarnessId,
        #[serde(default)]
        title: Option<String>,
    },
    SendMessage {
        thread_id: ThreadId,
        text: String,
    },
    Interrupt {
        thread_id: ThreadId,
    },
    RespondRequest {
        thread_id: ThreadId,
        request_id: RequestId,
        decision: DecisionKind,
    },
    SetPermissionMode {
        thread_id: ThreadId,
        mode: Option<PermissionMode>,
    },
    /// Ask the daemon to exit (graceful shutdown or upgrade).
    Shutdown {
        reason: ShutdownReason,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    User,
    Upgrade,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Topic {
    /// Projects and thread summaries.
    Workspace,
    /// Full timeline of one thread.
    Thread(ThreadId),
}

// ---------------------------------------------------------------------------
// Daemon → client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome(Welcome),
    Response {
        id: u64,
        result: Result<Response, ErrorBody>,
    },
    Snapshot {
        sub_id: u64,
        snapshot: Snapshot,
    },
    Event {
        sub_id: u64,
        event: EventEnvelope,
    },
    Delta {
        sub_id: u64,
        delta: Delta,
    },
    /// Connection-level error (bad handshake, unsupported version…).
    Fatal(ErrorBody),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Welcome {
    pub protocol_version: u32,
    pub daemon_build: String,
    pub daemon_pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Ok,
    Project { project: Project },
    Thread { thread: Thread },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorBody {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Conflict,
    Unsupported,
    Harness,
    Internal,
    IncompatibleVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // wire type, built once per subscription
pub enum Snapshot {
    Workspace {
        seq: u64,
        projects: Vec<Project>,
        threads: Vec<Thread>,
    },
    Thread {
        seq: u64,
        thread: Thread,
        turns: Vec<Turn>,
        items: Vec<Item>,
        requests: Vec<RuntimeRequest>,
    },
}

/// A committed, persisted domain event with its global sequence number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub seq: u64,
    pub at_ms: i64,
    pub event: Event,
}

/// Domain events. Persisted in the daemon's event log and fanned out to
/// subscribers. Each variant carries the full new state of the entity it
/// touches, so clients can apply them without extra queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ProjectAdded { project: Project },
    ThreadCreated { thread: Thread },
    ThreadUpdated { thread: Thread },
    TurnStarted { turn: Turn },
    TurnUpdated { turn: Turn },
    ItemUpserted { item: Item },
    RequestOpened { request: RuntimeRequest },
    RequestUpdated { request: RuntimeRequest },
}

impl Event {
    /// The thread this event belongs to, if any (workspace-level otherwise).
    pub fn thread_id(&self) -> Option<&ThreadId> {
        match self {
            Event::ProjectAdded { .. } => None,
            Event::ThreadCreated { thread } | Event::ThreadUpdated { thread } => Some(&thread.id),
            Event::TurnStarted { turn } | Event::TurnUpdated { turn } => Some(&turn.thread_id),
            Event::ItemUpserted { item } => Some(&item.thread_id),
            Event::RequestOpened { request } | Event::RequestUpdated { request } => {
                Some(&request.thread_id)
            }
        }
    }

    /// Whether workspace subscribers should receive this event.
    pub fn is_workspace_event(&self) -> bool {
        matches!(
            self,
            Event::ProjectAdded { .. } | Event::ThreadCreated { .. } | Event::ThreadUpdated { .. }
        )
    }
}

/// Ephemeral streaming chunk. Not persisted; the item snapshot catches up.
///
/// `offset` is the length (in bytes) of the item's streamed text *before* this
/// chunk, so clients can drop chunks they already have from a snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Delta {
    pub thread_id: ThreadId,
    pub item_id: ItemId,
    pub channel: DeltaChannel,
    pub offset: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeltaChannel {
    Text,
    Reasoning,
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_msg_round_trip() {
        let msgs = vec![
            ClientMsg::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                client_kind: ClientKind::LocalUi,
                client_build: "test".into(),
            }),
            ClientMsg::Request {
                id: 7,
                body: Request::SendMessage {
                    thread_id: ThreadId::new(),
                    text: "hi".into(),
                },
            },
            ClientMsg::Subscribe {
                sub_id: 1,
                topic: Topic::Thread(ThreadId::new()),
            },
        ];
        for msg in msgs {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back, "{json}");
        }
    }

    #[test]
    fn server_msg_round_trip() {
        let msg = ServerMsg::Response {
            id: 1,
            result: Err(ErrorBody::new(ErrorCode::NotFound, "no such thread")),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<ServerMsg>(&json).unwrap(), msg);
    }
}
