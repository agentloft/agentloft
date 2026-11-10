//! What a remote session (a browser signed in to web access) may ask for
//! (ADR-0016).
//!
//! Local clients reach the daemon over its Unix socket, as the user who owns
//! the data home, and may do everything. A remote session is held to an
//! allow-list: it gets what the web UI needs, and nothing that would let it
//! widen its own reach — no daemon shutdown, no web-access management, no
//! agent binary paths, launch arguments or environment, and no file outside
//! the places Agentloft already works in.
//!
//! This is hygiene, not a sandbox: a signed-in session can still ask an
//! agent to run commands, which is the point of the product. The limits keep
//! a stolen session from becoming something more permanent or wider than
//! that.

use crate::{Core, CoreResult, expand_tilde, internal};
use loft_protocol::{
    ErrorBody, ErrorCode, FileContent, FsRoot, Request, Response, ThreadId, Topic,
};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The switches a remote session's limits depend on (`[web]` in config).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemotePolicy {
    /// Terminals: a shell on the daemon's machine.
    pub allow_terminal: bool,
    /// The daemon's browser: pages loaded on its machine.
    pub allow_browser: bool,
}

fn forbidden(what: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Forbidden,
        format!("{what} isn't available in the browser. Use the desktop app."),
    )
}

/// A terminal asked for by a session that isn't given one (`[web]
/// allow_terminal`).
fn terminals_off() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Forbidden,
        "Terminals are off for browsers. Turn them on in the desktop app: Settings ▸ Web access.",
    )
}

/// The daemon's browser asked for by a session that isn't given it (`[web]
/// allow_browser`).
fn browser_off() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Forbidden,
        "The browser panel is off here. Turn it on in the desktop app: Settings ▸ Web access.",
    )
}

/// Settings a remote session may change. Everything else — where agent
/// binaries live, their arguments and environment, project folders, web
/// access itself — stays with the desktop app.
fn remote_may_set(key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["ui", ..] | ["worktrees", _] | ["titles", _] => true,
        // How the browser tab behaves; not which program is run for it.
        ["browser", "search" | "home"] => true,
        ["harness", _, field] => matches!(
            *field,
            "enabled"
                | "default_model"
                | "default_effort"
                | "default_permission_mode"
                | "hidden_models"
        ),
        _ => false,
    }
}

/// Resolve `path` to where it really is, following symlinks, even when its
/// last components don't exist yet (a folder about to be created). `..` is
/// refused in the part that doesn't exist, where it can't be resolved.
fn real_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(real) => {
                let mut out = real;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return Some(out);
            }
            Err(_) => {
                let name = existing.file_name()?.to_owned();
                if !matches!(
                    Path::new(&name).components().next(),
                    Some(Component::Normal(_))
                ) {
                    return None;
                }
                rest.push(name);
                if !existing.pop() {
                    return None;
                }
            }
        }
    }
}

/// `path` resolved (symlinks and all), if it lands inside one of `roots`.
pub(crate) fn confine(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let real = real_path(path)?;
    roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| real.starts_with(&root))
        .then_some(real)
}

impl Core {
    fn home_dir(&self) -> Option<PathBuf> {
        self.opts()
            .config_file
            .as_ref()
            .and_then(|f| f.parent())
            .map(Path::to_path_buf)
    }

    /// Folders Agentloft works in: every project and every thread's folder
    /// (worktrees included).
    fn work_roots(&self) -> CoreResult<Vec<PathBuf>> {
        let (projects, threads) = self.with_store(|s| Ok((s.projects()?, s.threads()?)))?;
        let mut roots: Vec<PathBuf> = projects
            .into_iter()
            .map(|p| PathBuf::from(p.root_path))
            .collect();
        roots.extend(
            threads
                .into_iter()
                .filter(|t| !t.cwd.is_empty())
                .map(|t| PathBuf::from(t.cwd)),
        );
        Ok(roots)
    }

    /// Where a remote session may read files by path: the work folders, plus
    /// the attachments and icons Agentloft keeps.
    fn readable_roots(&self) -> CoreResult<Vec<PathBuf>> {
        let mut roots = self.work_roots()?;
        roots.push(self.opts().attachments_dir.clone());
        if let Some(home) = self.home_dir() {
            roots.push(home.join("icons"));
        }
        Ok(roots)
    }

    /// Where a remote session may add, create or clone projects: the folder
    /// new projects go in, and the projects folder if the user set one.
    fn project_parents(&self) -> Vec<PathBuf> {
        let config = self.config();
        let mut parents = vec![self.opts().projects_dir.clone()];
        for dir in [&config.projects.new_dir, &config.projects.base_dir] {
            if let Some(dir) = dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                parents.push(expand_tilde(dir));
            }
        }
        parents
    }

    /// A file by its path on this machine (`Request::ReadPath`). With
    /// `confined`, only inside [`Core::readable_roots`].
    pub(crate) fn read_path(
        &self,
        path: &str,
        thread_id: Option<&ThreadId>,
        confined: bool,
    ) -> CoreResult<FileContent> {
        let not_found = || ErrorBody::new(ErrorCode::NotFound, "no such file");
        let given = path.strip_prefix("file://").unwrap_or(path);
        let mut full = expand_tilde(given);
        if !full.is_absolute() {
            let thread = thread_id.ok_or_else(not_found)?;
            full = self.root_dir(&FsRoot::Thread(thread.clone()))?.join(full);
        }
        let full = if confined {
            // One answer for "outside" and "missing": a remote session can't
            // probe which files exist elsewhere.
            confine(&full, &self.readable_roots()?).ok_or_else(not_found)?
        } else {
            full
        };
        let meta = std::fs::metadata(&full).map_err(|_| not_found())?;
        if !meta.is_file() {
            return Err(not_found());
        }
        self.read_file_at(&full, path)
    }

    /// Refuse what a remote session may not ask for; `Ok` means "go on".
    fn check_remote(&self, req: &Request, policy: &RemotePolicy) -> CoreResult<()> {
        let inside = |path: &str, roots: &[PathBuf], what: &str| -> CoreResult<()> {
            confine(&expand_tilde(path), roots)
                .map(|_| ())
                .ok_or_else(|| forbidden(what))
        };
        match req {
            // Thread and conversation work: the product itself.
            Request::Ping
            | Request::CreateChat { .. }
            | Request::CreateThread { .. }
            | Request::SendMessage { .. }
            | Request::Steer { .. }
            | Request::Interrupt { .. }
            | Request::AgentInfo { .. }
            | Request::StopSubagent { .. }
            | Request::RespondRequest { .. }
            | Request::SetPermissionMode { .. }
            | Request::SetModel { .. }
            | Request::SetEffort { .. }
            | Request::SetThreadHarness { .. }
            | Request::SetThreadWorkspace { .. }
            | Request::ListBranches { .. }
            | Request::MergeThread { .. }
            | Request::ForkThread { .. }
            | Request::RevertTurn { .. }
            | Request::RenameThread { .. }
            | Request::AskOnce { .. }
            | Request::SetDraft { .. }
            | Request::UnarchiveThread { .. }
            | Request::CreatePullRequest { .. }
            | Request::ArchiveThread { .. }
            | Request::DeleteThread { .. }
            | Request::SearchThreads { .. }
            // Projects that already exist.
            | Request::RenameProject { .. }
            | Request::SetProjectDefaults { .. }
            | Request::RemoveProject { .. }
            | Request::GitHubLogin
            // Reads scoped to a project or thread folder by `FsRoot`.
            | Request::ListDir { .. }
            | Request::ReadFile { .. }
            | Request::FindFiles { .. }
            | Request::GetDiff { .. }
            // Confined in `handle_remote`.
            | Request::ReadPath { .. }
            // The settings files (secrets hidden in `handle_remote`), shared
            // UI state, and icons the daemon finds itself.
            | Request::UiFiles { .. }
            | Request::PutUiState { .. }
            | Request::DeleteTheme { .. }
            | Request::ProjectIcon { .. }
            // Settings: read (secrets hidden) and the harmless keys.
            | Request::GetSettings
            | Request::SaveTheme { .. }
            | Request::ReloadConfig
            | Request::ProbeHarness { .. }
            | Request::ListModels { .. } => Ok(()),

            Request::ChangeSetting { key, .. } => {
                if remote_may_set(key) {
                    Ok(())
                } else {
                    Err(forbidden(&format!("Changing `{key}`")))
                }
            }

            // New projects only where projects are kept.
            Request::AddProject { path, .. } => {
                inside(path, &self.project_parents(), "Adding a folder from elsewhere")
            }
            Request::CreateProject { parent, .. } => match parent {
                Some(parent) => inside(
                    parent,
                    &self.project_parents(),
                    "Creating a project elsewhere",
                ),
                None => Ok(()),
            },
            Request::CloneProject { parent, .. } => {
                inside(parent, &self.project_parents(), "Cloning elsewhere")
            }
            // An icon is copied into Agentloft's home and then readable:
            // only from the project's own folder.
            Request::SetProjectIcon { project_id, icon } => match icon.as_deref().map(str::trim) {
                None | Some("") => Ok(()),
                Some(icon) => {
                    let root = self.root_dir(&FsRoot::Project(project_id.clone()))?;
                    inside(icon, &[root], "An icon from outside the project")
                }
            },
            // Starts the agent's CLI in `cwd`.
            Request::ListCommands { cwd, .. } => {
                inside(cwd, &self.work_roots()?, "Listing commands there")
            }

            Request::TerminalOpen { .. }
            | Request::TerminalInput { .. }
            | Request::TerminalResize { .. }
            | Request::TerminalClose { .. }
            | Request::ListTerminals { .. } => {
                if policy.allow_terminal {
                    Ok(())
                } else {
                    Err(terminals_off())
                }
            }

            Request::PageOpen { .. }
            | Request::PageNavigate { .. }
            | Request::PageInput { .. }
            | Request::PageResize { .. }
            | Request::PageSelection { .. }
            | Request::PageClose { .. } => {
                if policy.allow_browser {
                    Ok(())
                } else {
                    Err(browser_off())
                }
            }

            Request::Shutdown { .. } => Err(forbidden("Stopping Agentloft")),
            Request::WebStatus
            | Request::WebRotateToken
            | Request::WebRevokeSession { .. }
            | Request::WebRevokeAllSessions => Err(forbidden("Managing web access")),
            // Agent-facing requests (the Agentloft MCP server); the UI
            // doesn't use them.
            Request::BrowserCall { .. }
            | Request::SpawnThread { .. }
            | Request::ListProjects
            | Request::ThreadOutcome { .. }
            | Request::ListThreads { .. }
            | Request::ReadThread { .. } => Err(forbidden("That request")),
        }
    }

    /// [`Core::handle`] for a remote session.
    pub async fn handle_remote(
        self: &Arc<Self>,
        req: Request,
        policy: &RemotePolicy,
    ) -> CoreResult<Response> {
        self.check_remote(&req, policy)?;
        match req {
            Request::ReadPath { path, thread_id } => {
                let core = self.clone();
                let file = tokio::task::spawn_blocking(move || {
                    core.read_path(&path, thread_id.as_ref(), true)
                })
                .await
                .map_err(internal)??;
                Ok(Response::File { file })
            }
            Request::GetSettings => self.handle(Request::GetSettings).await.map(redact_settings),
            Request::UiFiles { known } => Ok(Response::UiFiles {
                files: self.ui_files(known.as_deref(), true)?.map(Box::new),
            }),
            other => self.handle(other).await,
        }
    }

    /// [`Core::handle_inline`] for a remote session.
    pub fn handle_inline_remote(
        &self,
        req: &Request,
        policy: &RemotePolicy,
    ) -> Option<CoreResult<Response>> {
        match req {
            Request::TerminalInput { .. }
            | Request::TerminalResize { .. }
            | Request::PageInput { .. } => Some(match self.check_remote(req, policy) {
                Ok(()) => self.handle_inline(req)?,
                Err(e) => Err(e),
            }),
            _ => None,
        }
    }

    /// Whether a remote session may subscribe to `topic`.
    pub fn check_remote_topic(&self, topic: &Topic, policy: &RemotePolicy) -> CoreResult<()> {
        match topic {
            Topic::Workspace | Topic::Thread(_) => Ok(()),
            Topic::Terminal(_) if policy.allow_terminal => Ok(()),
            Topic::Terminal(_) => Err(terminals_off()),
            Topic::Page(_) if policy.allow_browser => Ok(()),
            Topic::Page(_) => Err(browser_off()),
        }
    }
}

/// Settings as a remote session sees them: agents' environment variables
/// (often API keys) are named but not shown, and web access is left out.
fn redact_settings(response: Response) -> Response {
    let Response::Json { mut value } = response else {
        return response;
    };
    if let Some(current) = value.get_mut("current").and_then(|c| c.as_object_mut()) {
        current.remove("web");
        if let Some(agents) = current.get_mut("harness").and_then(|h| h.as_object_mut()) {
            for agent in agents.values_mut() {
                if let Some(env) = agent.get_mut("env").and_then(|e| e.as_object_mut()) {
                    for v in env.values_mut() {
                        *v = serde_json::Value::String("•••".into());
                    }
                }
            }
        }
    }
    Response::Json { value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CoreOptions;
    use loft_harness::fake::FakeHarness;
    use loft_protocol::{HarnessId, ProjectId};
    use loft_store::Store;

    struct Fixture {
        core: Arc<Core>,
        dir: tempfile::TempDir,
        project: PathBuf,
        project_id: ProjectId,
    }

    /// A home with a config file, one project (`<tmp>/work/app`) and one
    /// thread in it.
    async fn fixture() -> (Fixture, ThreadId) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("work/app");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[harness.claude]\nenv = { ANTHROPIC_API_KEY = \"sk-secret\" }\n\n[web]\nenabled = true\n",
        )
        .unwrap();
        let core = Core::new(
            Store::open(&dir.path().join("db.sqlite")).unwrap(),
            vec![Arc::new(FakeHarness)],
            CoreOptions {
                config_file: Some(home.join("config.toml")),
                attachments_dir: home.join("attachments"),
                projects_dir: home.join("projects"),
                ..Default::default()
            },
        );
        let Response::Project { project: p } = core
            .handle(Request::AddProject {
                path: project.to_string_lossy().into_owned(),
                name: None,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        let Response::Thread { thread } = core
            .handle(Request::CreateThread {
                project_id: p.id.clone(),
                harness: HarnessId(HarnessId::FAKE.into()),
                title: None,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        (
            Fixture {
                core,
                dir,
                project,
                project_id: p.id,
            },
            thread.id,
        )
    }

    const WEB: RemotePolicy = RemotePolicy {
        allow_terminal: false,
        allow_browser: false,
    };

    async fn remote(f: &Fixture, req: Request) -> CoreResult<Response> {
        f.core.handle_remote(req, &WEB).await
    }

    fn is_forbidden<T: std::fmt::Debug>(r: &CoreResult<T>) -> bool {
        matches!(r, Err(e) if e.code == ErrorCode::Forbidden)
    }

    fn read(path: impl Into<String>, thread_id: Option<&ThreadId>) -> Request {
        Request::ReadPath {
            path: path.into(),
            thread_id: thread_id.cloned(),
        }
    }

    #[tokio::test]
    async fn reads_stay_inside_work_folders_and_agentloft_files() {
        let (f, thread) = fixture().await;
        std::fs::write(f.project.join("logo.png"), b"png").unwrap();
        let attachments = f.dir.path().join("home/attachments");
        std::fs::create_dir_all(&attachments).unwrap();
        std::fs::write(attachments.join("shot.png"), b"shot").unwrap();
        let secret = f.dir.path().join("secret.txt");
        std::fs::write(&secret, b"hunter2").unwrap();

        // Inside the project: absolute, and relative to the thread's folder.
        let abs = f.project.join("logo.png").to_string_lossy().into_owned();
        for req in [read(&abs, None), read("logo.png", Some(&thread))] {
            let Response::File { file } = remote(&f, req).await.unwrap() else {
                panic!()
            };
            assert_eq!(file.bytes.0, b"png");
        }
        // Agentloft's attachments.
        let shot = attachments.join("shot.png").to_string_lossy().into_owned();
        assert!(
            remote(&f, read(format!("file://{shot}"), None))
                .await
                .is_ok()
        );

        // Outside: refused, and indistinguishable from a missing file.
        let outside = remote(&f, read(secret.to_string_lossy(), None))
            .await
            .unwrap_err();
        let missing = remote(&f, read("/nonexistent/x.png", None))
            .await
            .unwrap_err();
        assert_eq!(outside, missing);
        assert_eq!(outside.code, ErrorCode::NotFound);
        for path in ["/etc/passwd", "/etc/hosts", "~/.ssh/id_ed25519"] {
            assert!(remote(&f, read(path, None)).await.is_err(), "{path}");
        }
        // The home's own secrets aren't in a readable folder.
        let config = f.dir.path().join("home/config.toml");
        assert!(
            remote(&f, read(config.to_string_lossy(), None))
                .await
                .is_err()
        );

        // A local client isn't confined.
        assert!(
            f.core
                .handle(read(secret.to_string_lossy(), None))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn dot_dot_and_symlinks_cannot_escape() {
        let (f, thread) = fixture().await;
        let secret = f.dir.path().join("secret.txt");
        std::fs::write(&secret, b"hunter2").unwrap();

        // `..` out of the project, absolute or relative.
        let dotted = format!("{}/../../secret.txt", f.project.display());
        assert!(remote(&f, read(dotted, None)).await.is_err());
        assert!(
            remote(&f, read("../../secret.txt", Some(&thread)))
                .await
                .is_err()
        );
        // Even `..` that would land back inside is refused.
        std::fs::create_dir_all(f.project.join("sub")).unwrap();
        std::fs::write(f.project.join("ok.txt"), b"ok").unwrap();
        let back_inside = format!("{}/sub/../ok.txt", f.project.display());
        assert!(remote(&f, read(back_inside, None)).await.is_err());

        #[cfg(unix)]
        {
            // A symlink inside the project pointing out of it.
            std::os::unix::fs::symlink(&secret, f.project.join("link.txt")).unwrap();
            let link = f.project.join("link.txt").to_string_lossy().into_owned();
            assert!(remote(&f, read(&link, None)).await.is_err());
            assert!(remote(&f, read("link.txt", Some(&thread))).await.is_err());
            // A symlinked directory, likewise.
            std::os::unix::fs::symlink(f.dir.path(), f.project.join("up")).unwrap();
            let via_dir = f
                .project
                .join("up/secret.txt")
                .to_string_lossy()
                .into_owned();
            assert!(remote(&f, read(via_dir, None)).await.is_err());
            // A symlink that stays inside is fine.
            std::os::unix::fs::symlink(f.project.join("ok.txt"), f.project.join("alias.txt"))
                .unwrap();
            let alias = f.project.join("alias.txt").to_string_lossy().into_owned();
            assert!(remote(&f, read(alias, None)).await.is_ok());
        }
    }

    #[tokio::test]
    async fn projects_only_where_projects_are_kept() {
        let (f, _) = fixture().await;
        let elsewhere = f.dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let add = |path: &Path| Request::AddProject {
            path: path.to_string_lossy().into_owned(),
            name: None,
        };
        assert!(is_forbidden(&remote(&f, add(&elsewhere)).await));
        assert!(is_forbidden(&remote(&f, add(Path::new("/"))).await));
        assert!(is_forbidden(
            &remote(
                &f,
                Request::CloneProject {
                    parent: elsewhere.to_string_lossy().into_owned(),
                    source: "https://example.com/x.git".into(),
                    github: false,
                }
            )
            .await
        ));
        assert!(is_forbidden(
            &remote(
                &f,
                Request::CreateProject {
                    parent: Some(elsewhere.to_string_lossy().into_owned()),
                    name: "x".into(),
                    github: false,
                }
            )
            .await
        ));

        // Under the folder new projects go in: allowed.
        let kept = f.dir.path().join("home/projects/mine");
        std::fs::create_dir_all(&kept).unwrap();
        assert!(remote(&f, add(&kept)).await.is_ok());
        // …but not by climbing out of it.
        let climbing = f.dir.path().join("home/projects/../../elsewhere");
        assert!(is_forbidden(&remote(&f, add(&climbing)).await));
    }

    #[tokio::test]
    async fn icons_and_commands_are_confined_too() {
        let (f, _) = fixture().await;
        let secret = f.dir.path().join("secret.png");
        std::fs::write(&secret, b"x").unwrap();
        let icon = |path: Option<String>| Request::SetProjectIcon {
            project_id: f.project_id.clone(),
            icon: path,
        };
        assert!(is_forbidden(
            &remote(&f, icon(Some(secret.to_string_lossy().into_owned()))).await
        ));
        std::fs::write(f.project.join("icon.png"), b"x").unwrap();
        let own = f.project.join("icon.png").to_string_lossy().into_owned();
        assert!(remote(&f, icon(Some(own))).await.is_ok());
        assert!(remote(&f, icon(None)).await.is_ok());

        assert!(is_forbidden(
            &remote(
                &f,
                Request::ListCommands {
                    harness: HarnessId(HarnessId::FAKE.into()),
                    cwd: "/".into(),
                }
            )
            .await
        ));
    }

    #[tokio::test]
    async fn settings_are_limited_and_secrets_hidden() {
        let (f, _) = fixture().await;
        let set = |key: &str, value: serde_json::Value| Request::ChangeSetting {
            key: key.into(),
            value,
        };
        for key in [
            "harness.claude.path",
            "harness.codex.extra_args",
            "harness.claude.env",
            "projects.base_dir",
            "projects.new_dir",
            "web.enabled",
            "web.allow_insecure_remote",
            "web.bind",
            // Which program the daemon runs as its browser.
            "browser.executable",
        ] {
            let r = remote(&f, set(key, serde_json::json!("x"))).await;
            assert!(is_forbidden(&r), "{key}: {r:?}");
        }
        assert!(
            remote(&f, set("ui.theme", serde_json::json!("dark")))
                .await
                .is_ok()
        );
        // How the browser tab behaves is the viewer's to choose.
        for (key, value) in [
            ("browser.search", "google"),
            ("browser.home", "https://example.com"),
        ] {
            let r = remote(&f, set(key, serde_json::json!(value))).await;
            assert!(r.is_ok(), "{key}: {r:?}");
        }
        assert_eq!(
            f.core.config().browser.search_template(),
            "https://www.google.com/search?q={}"
        );
        // The desktop app may say which browser to run.
        let local = f
            .core
            .handle(set("browser.executable", serde_json::json!("/opt/chrome")))
            .await;
        assert!(local.is_ok(), "{local:?}");
        assert_eq!(
            f.core.config().browser.executable.as_deref(),
            Some("/opt/chrome")
        );
        assert!(
            remote(
                &f,
                set("harness.claude.default_model", serde_json::json!("opus"))
            )
            .await
            .is_ok()
        );

        let Response::Json { value } = remote(&f, Request::GetSettings).await.unwrap() else {
            panic!()
        };
        let text = value.to_string();
        assert!(!text.contains("sk-secret"), "{text}");
        assert!(text.contains("ANTHROPIC_API_KEY"));
        assert!(value["current"].get("web").is_none());
        // A local client sees its own settings as they are.
        let Response::Json { value } = f.core.handle(Request::GetSettings).await.unwrap() else {
            panic!()
        };
        assert!(value.to_string().contains("sk-secret"));
    }

    #[tokio::test]
    async fn settings_files_reach_the_browser_without_secrets() {
        let (f, _) = fixture().await;
        let home = f.dir.path().join("home");
        std::fs::create_dir_all(home.join("themes")).unwrap();
        std::fs::write(home.join("themes/mine.toml"), "name = \"Mine\"\n").unwrap();

        let Response::UiFiles { files: Some(files) } =
            remote(&f, Request::UiFiles { known: None }).await.unwrap()
        else {
            panic!()
        };
        assert!(!files.config_toml.contains("sk-secret"));
        assert!(files.config_toml.contains("ANTHROPIC_API_KEY"));
        assert!(!files.config_toml.contains("[web]"));
        assert_eq!(files.themes.len(), 1);
        assert_eq!(files.themes[0].name, "mine");
        assert_eq!(std::path::Path::new(&files.home), home);
        assert!(files.sidebar.is_none());

        // Unchanged since: nothing is sent again.
        let again = remote(
            &f,
            Request::UiFiles {
                known: Some(files.stamp.clone()),
            },
        )
        .await
        .unwrap();
        assert_eq!(again, Response::UiFiles { files: None });

        // Sidebar state is saved, and shows up as a change.
        let put = |name: &str, json: &str| Request::PutUiState {
            name: name.into(),
            json: json.into(),
        };
        remote(&f, put("sidebar", "{\"collapsed\":[]}"))
            .await
            .unwrap();
        let Response::UiFiles { files: Some(after) } = remote(
            &f,
            Request::UiFiles {
                known: Some(files.stamp),
            },
        )
        .await
        .unwrap() else {
            panic!()
        };
        assert_eq!(after.sidebar.as_deref(), Some("{\"collapsed\":[]}"));
        // Only the named state, only JSON, only so much of it.
        assert!(remote(&f, put("../config", "{}")).await.is_err());
        assert!(remote(&f, put("session", "{}")).await.is_err());
        assert!(remote(&f, put("sidebar", "not json")).await.is_err());
        let huge = format!("\"{}\"", "x".repeat(300 * 1024));
        assert!(remote(&f, put("sidebar", &huge)).await.is_err());

        // A local client gets the config as it is.
        let Response::UiFiles { files: Some(local) } = f
            .core
            .handle(Request::UiFiles { known: None })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(local.config_toml.contains("sk-secret"));
    }

    #[tokio::test]
    async fn themes_are_deleted_by_name_only() {
        let (f, _) = fixture().await;
        let home = f.dir.path().join("home");
        std::fs::create_dir_all(home.join("themes")).unwrap();
        std::fs::write(home.join("themes/mine.toml"), "name = \"Mine\"\n").unwrap();
        let delete = |id: &str| Request::DeleteTheme { id: id.into() };
        for id in [
            "custom:../config",
            "custom:../../secret",
            "custom:mine/../../config",
            "custom:",
            "/etc/passwd",
            "custom:Mine.toml",
        ] {
            assert!(remote(&f, delete(id)).await.is_err(), "{id}");
        }
        assert!(home.join("config.toml").exists());
        assert!(remote(&f, delete("custom:mine")).await.is_ok());
        assert!(!home.join("themes/mine.toml").exists());
        assert!(
            remote(&f, delete("custom:mine")).await.is_err(),
            "already gone"
        );
    }

    #[tokio::test]
    async fn a_projects_icon_is_found_by_the_daemon() {
        let (f, _) = fixture().await;
        let icon = |id: &ProjectId| Request::ProjectIcon {
            project_id: id.clone(),
        };
        let none = remote(&f, icon(&f.project_id)).await.unwrap_err();
        assert_eq!(none.code, ErrorCode::NotFound);
        std::fs::write(f.project.join("favicon.png"), b"png!").unwrap();
        let Response::File { file } = remote(&f, icon(&f.project_id)).await.unwrap() else {
            panic!()
        };
        assert_eq!(file.bytes.0, b"png!");
        assert!(remote(&f, icon(&ProjectId::new())).await.is_err());
    }

    #[tokio::test]
    async fn terminals_shutdown_and_management_are_refused() {
        let (f, thread) = fixture().await;
        let open = || Request::TerminalOpen {
            root: FsRoot::Thread(thread.clone()),
            cols: 80,
            rows: 24,
            key: None,
        };
        assert!(is_forbidden(&remote(&f, open()).await));
        let id = loft_protocol::TerminalId::new();
        let input = Request::TerminalInput {
            id: id.clone(),
            data: b"id\n".to_vec().into(),
        };
        assert!(is_forbidden(
            &f.core.handle_inline_remote(&input, &WEB).unwrap()
        ));
        assert!(
            f.core
                .check_remote_topic(&Topic::Terminal(id.clone()), &WEB)
                .is_err()
        );
        assert!(f.core.check_remote_topic(&Topic::Workspace, &WEB).is_ok());

        // Allowed once `[web] allow_terminal` is on.
        let with_terminal = RemotePolicy {
            allow_terminal: true,
            ..WEB
        };
        assert!(f.core.check_remote(&open(), &with_terminal).is_ok());
        assert!(
            f.core
                .check_remote_topic(&Topic::Terminal(id), &with_terminal)
                .is_ok()
        );

        for req in [
            Request::Shutdown {
                reason: loft_protocol::ShutdownReason::User,
            },
            Request::WebStatus,
            Request::WebRotateToken,
            Request::WebRevokeAllSessions,
            Request::WebRevokeSession { id: "x".into() },
            Request::ListProjects,
            Request::ListThreads {
                query: None,
                project_id: None,
            },
        ] {
            let r = remote(&f, req.clone()).await;
            assert!(is_forbidden(&r), "{req:?}: {r:?}");
        }
        // Ordinary work goes through.
        assert!(remote(&f, Request::Ping).await.is_ok());
        assert!(
            remote(
                &f,
                Request::ListDir {
                    root: FsRoot::Thread(thread),
                    rel_path: String::new(),
                }
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn the_daemons_browser_is_refused_unless_allowed() {
        let (f, thread) = fixture().await;
        let id = loft_protocol::PageId::new();
        let asks = || {
            [
                Request::PageOpen {
                    thread_id: thread.clone(),
                    url: Some("https://example.com".into()),
                    width: 800,
                    height: 600,
                    scale: 1.0,
                },
                Request::PageNavigate {
                    id: id.clone(),
                    nav: loft_protocol::PageNav::Reload,
                },
                Request::PageInput {
                    id: id.clone(),
                    input: loft_protocol::PageInput::Text { text: "x".into() },
                },
                Request::PageResize {
                    id: id.clone(),
                    width: 800,
                    height: 600,
                    scale: 1.0,
                },
                Request::PageSelection { id: id.clone() },
                Request::PageClose { id: id.clone() },
            ]
        };
        for req in asks() {
            assert!(
                is_forbidden(&f.core.check_remote(&req, &WEB)),
                "{req:?} without the right"
            );
        }
        // Input takes the ordered path; the check is on it too.
        let typed = f.core.handle_inline_remote(&asks()[2], &WEB);
        assert!(matches!(&typed, Some(r) if is_forbidden(r)), "{typed:?}");
        assert!(
            f.core
                .check_remote_topic(&Topic::Page(id.clone()), &WEB)
                .is_err()
        );

        // Allowed once `[web] allow_browser` is on. Nothing is open, so
        // what goes through finds no page, which is not "forbidden".
        let with_browser = RemotePolicy {
            allow_browser: true,
            ..WEB
        };
        for req in asks() {
            assert!(f.core.check_remote(&req, &with_browser).is_ok(), "{req:?}");
        }
        assert!(
            f.core
                .check_remote_topic(&Topic::Page(id.clone()), &with_browser)
                .is_ok()
        );
        let missing = f
            .core
            .handle_remote(Request::PageSelection { id: id.clone() }, &with_browser)
            .await;
        assert!(
            matches!(&missing, Err(e) if e.code == ErrorCode::NotFound),
            "{missing:?}"
        );
        // A terminal's right is not the browser's.
        let with_terminal = RemotePolicy {
            allow_terminal: true,
            ..WEB
        };
        assert!(is_forbidden(
            &f.core.check_remote(&asks()[0], &with_terminal)
        ));
    }

    #[test]
    fn real_path_handles_missing_tails() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        assert_eq!(
            real_path(&dir.path().join("new/child")).unwrap(),
            base.join("new/child")
        );
        assert!(real_path(Path::new("relative/path")).is_none());
        assert!(confine(&dir.path().join("a/../b"), &[dir.path().to_path_buf()]).is_none());
        assert!(confine(&dir.path().join("a/b"), &[dir.path().to_path_buf()]).is_some());
        assert!(confine(dir.path(), &[dir.path().join("a")]).is_none());
    }
}
