//! What an agent asks of a web page, and how it is done there, without any
//! UI: the [`BrowserCommand`]s and their typed [`BrowserError`]s, address
//! handling ([`normalize_address`]), the helper scripts the page runs
//! (`js/agent.js`, through [`script`]) and the page outline an agent reads
//! ([`format_snapshot`]).
//!
//! Two browsers run these: the daemon's headless Chrome
//! (`loft-core`'s `pages::automation`, over the DevTools protocol), and the
//! desktop app's WKWebView panel (`loft-browser`, which re-exports this
//! crate). Both answer each command with the same JSON (see `loft-browser`'s
//! crate docs for the table).

pub mod address;
pub mod capture;
pub mod command;
pub mod script;
pub mod snapshot;

pub use address::{
    DEFAULT_SEARCH_TEMPLATE, SchemePolicy, normalize_address, scheme_policy, search_url,
};
pub use capture::{
    Clip, ConsoleReadOptions, DiffOptions, EmulateOptions, ImageFormat, NetworkOptions,
    NetworkStopOptions, RecordingOptions, Region, ScreenshotOptions, ShotScale,
};
pub use command::{
    BrowserCommand, BrowserError, DEFAULT_WAIT, EVAL_TIMEOUT, EXECUTE_TIMEOUT, MAX_EXECUTE_TIMEOUT,
    MAX_WAIT, MouseButton, NAVIGATION_TIMEOUT, SCRIPT_TIMEOUT, ScrollEdge, TabsOp, Target,
};
pub use script::{Helper, MAX_EVAL_CHARS, action_result, eval_reply};
pub use snapshot::{
    Block, BlockKind, ElementInfo, FormattedSnapshot, FrameInfo, MAX_SNAPSHOT_CHARS, PageSnapshot,
    Part, Viewport, format_snapshot,
};
