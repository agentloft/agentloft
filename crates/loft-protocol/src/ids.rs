//! Strongly typed identifiers. App ids are UUIDv7 (time-ordered); provider
//! ("native") ids are opaque strings owned by the harness.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! ids_uuid {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::now_v7().to_string())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

ids_uuid!(ProjectId);
ids_uuid!(ThreadId);
ids_uuid!(TurnId);
ids_uuid!(ItemId);
ids_uuid!(RequestId);
ids_uuid!(TerminalId);
ids_uuid!(PageId);
ids_uuid!(AutomationId);

/// Which harness drives a thread: `claude`, `codex`, `acp:gemini`, …
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HarnessId(pub String);

impl HarnessId {
    pub const CLAUDE: &'static str = "claude";
    pub const CODEX: &'static str = "codex";
    pub const OPENCODE: &'static str = "opencode";
    pub const GEMINI: &'static str = "gemini";
    pub const FAKE: &'static str = "fake";

    pub fn claude() -> Self {
        Self(Self::CLAUDE.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
