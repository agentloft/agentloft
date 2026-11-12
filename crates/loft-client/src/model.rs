//! Client-side mirrors of daemon projections. Pure data; the UI wraps these in
//! GPUI entities.

use loft_protocol::{
    Automation, Delta, Event, EventEnvelope, Item, ItemId, Project, RequestStatus, RuntimeRequest,
    Snapshot, Thread, ThreadId, Turn, TurnStatus,
};

#[derive(Debug, Default, Clone)]
pub struct WorkspaceModel {
    pub seq: u64,
    pub projects: Vec<Project>,
    /// Most recently updated first.
    pub threads: Vec<Thread>,
    /// Oldest first.
    pub automations: Vec<Automation>,
    pub loaded: bool,
}

impl WorkspaceModel {
    pub fn apply_snapshot(&mut self, snapshot: Snapshot) {
        if let Snapshot::Workspace {
            seq,
            projects,
            threads,
            automations,
        } = snapshot
        {
            self.seq = seq;
            self.projects = projects;
            self.threads = threads;
            self.automations = automations;
            self.loaded = true;
            self.sort();
        }
    }

    /// Returns `true` if anything changed.
    pub fn apply_event(&mut self, env: &EventEnvelope) -> bool {
        if env.seq <= self.seq {
            return false;
        }
        self.seq = env.seq;
        match &env.event {
            Event::ProjectAdded { project } => {
                if !self.projects.iter().any(|p| p.id == project.id) {
                    self.projects.push(project.clone());
                }
            }
            Event::ProjectUpdated { project } => {
                match self.projects.iter_mut().find(|p| p.id == project.id) {
                    Some(p) => *p = project.clone(),
                    None => self.projects.push(project.clone()),
                }
            }
            Event::ProjectRemoved { project_id } => {
                self.projects.retain(|p| &p.id != project_id);
                self.threads.retain(|t| &t.project_id != project_id);
                self.automations
                    .retain(|a| &a.spec.project_id != project_id);
            }
            Event::AutomationUpdated { automation } => {
                match self.automations.iter_mut().find(|a| a.id == automation.id) {
                    Some(a) => *a = automation.clone(),
                    None => self.automations.push(automation.clone()),
                }
            }
            Event::AutomationDeleted { automation_id } => {
                self.automations.retain(|a| &a.id != automation_id);
            }
            Event::ThreadCreated { thread } | Event::ThreadUpdated { thread } => {
                match self.threads.iter_mut().find(|t| t.id == thread.id) {
                    Some(t) => *t = thread.clone(),
                    None => self.threads.push(thread.clone()),
                }
                self.sort();
            }
            Event::ThreadDeleted { thread_id } => {
                self.threads.retain(|t| &t.id != thread_id);
            }
            _ => return false,
        }
        true
    }

    pub fn thread(&self, id: &ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| &t.id == id)
    }

    fn sort(&mut self) {
        self.threads
            .sort_by_key(|t| std::cmp::Reverse(t.updated_at_ms));
    }
}

#[derive(Debug, Clone)]
pub struct ThreadModel {
    pub seq: u64,
    pub thread: Thread,
    pub turns: Vec<Turn>,
    /// Ordered by `ordinal`.
    pub items: Vec<Item>,
    pub requests: Vec<RuntimeRequest>,
    /// Messages waiting for the running turn to complete, in order (the
    /// daemon holds and sends them).
    pub queue: Vec<loft_protocol::QueuedMessage>,
}

impl ThreadModel {
    pub fn from_snapshot(snapshot: Snapshot) -> Option<Self> {
        match snapshot {
            Snapshot::Thread {
                seq,
                thread,
                turns,
                mut items,
                requests,
                queue,
            } => {
                items.sort_by_key(|i| i.ordinal);
                Some(Self {
                    seq,
                    thread,
                    turns,
                    items,
                    requests,
                    queue,
                })
            }
            _ => None,
        }
    }

    pub fn apply_event(&mut self, env: &EventEnvelope) -> bool {
        if env.seq <= self.seq {
            return false;
        }
        self.seq = env.seq;
        match &env.event {
            Event::ThreadCreated { thread } | Event::ThreadUpdated { thread } => {
                if thread.id != self.thread.id {
                    return false;
                }
                self.thread = thread.clone();
            }
            Event::TurnStarted { turn } | Event::TurnUpdated { turn } => {
                match self.turns.iter_mut().find(|t| t.id == turn.id) {
                    Some(t) => *t = turn.clone(),
                    None => self.turns.push(turn.clone()),
                }
            }
            Event::ItemUpserted { item } => self.upsert_item(item.clone()),
            Event::RequestOpened { request } | Event::RequestUpdated { request } => {
                match self.requests.iter_mut().find(|r| r.id == request.id) {
                    Some(r) => *r = request.clone(),
                    None => self.requests.push(request.clone()),
                }
            }
            Event::QueueUpdated { thread_id, queue } => {
                if thread_id != &self.thread.id {
                    return false;
                }
                self.queue = queue.clone();
            }
            Event::ProjectAdded { .. }
            | Event::ProjectUpdated { .. }
            | Event::ProjectRemoved { .. }
            | Event::ThreadDeleted { .. }
            | Event::AutomationUpdated { .. }
            | Event::AutomationDeleted { .. } => return false,
        }
        true
    }

    /// Apply a streaming chunk, de-duplicating by byte offset.
    pub fn apply_delta(&mut self, delta: &Delta) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| i.id == delta.item_id) else {
            return false;
        };
        let Some(buf) = item.body.stream_text_mut(delta.channel) else {
            return false;
        };
        let have = buf.len();
        let end = delta.offset + delta.text.len();
        if end <= have {
            return false; // already contained in the snapshot
        }
        if delta.offset > have {
            // A gap: we missed chunks. The next item snapshot will fix it.
            tracing::debug!(item = %delta.item_id, have, offset = delta.offset, "delta gap");
            return false;
        }
        let skip = have - delta.offset;
        match delta.text.get(skip..) {
            Some(rest) => buf.push_str(rest),
            None => return false,
        }
        true
    }

    fn upsert_item(&mut self, item: Item) {
        match self.items.iter_mut().find(|i| i.id == item.id) {
            Some(existing) => {
                // Keep locally streamed text if the event snapshot is older.
                *existing = item;
            }
            None => {
                let pos = self.items.partition_point(|i| i.ordinal <= item.ordinal);
                self.items.insert(pos, item);
            }
        }
    }

    pub fn running_turn(&self) -> Option<&Turn> {
        self.turns
            .iter()
            .rev()
            .find(|t| t.status == TurnStatus::Running)
    }

    pub fn open_requests(&self) -> impl Iterator<Item = &RuntimeRequest> {
        self.requests
            .iter()
            .filter(|r| r.status == RequestStatus::Open)
    }

    pub fn request_for_item(&self, item: &ItemId) -> Option<&RuntimeRequest> {
        self.requests
            .iter()
            .filter(|r| r.item_id.as_ref() == Some(item))
            .max_by_key(|r| r.opened_at_ms)
    }

    pub fn last_turn(&self) -> Option<&Turn> {
        self.turns.iter().max_by_key(|t| t.ordinal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::*;

    fn model_with_item(text: &str) -> (ThreadModel, ItemId) {
        let thread = Thread {
            id: ThreadId::new(),
            project_id: ProjectId::new(),
            title: "t".into(),
            harness: HarnessId::claude(),
            cwd: "/".into(),
            model: None,
            permission_mode: None,
            effort: None,
            native_session_id: None,
            status: ThreadStatus::Running,
            created_at_ms: 0,
            updated_at_ms: 0,
            workspace: Default::default(),
            parent: None,
            slash_commands: Vec::new(),
            command_info: Vec::new(),
            started_at_ms: None,
            draft: None,
            waiting_on: Vec::new(),
            limit: None,
            automation_id: None,
            pull_request: None,
            landed: false,
        };
        let item = Item {
            id: ItemId::new(),
            thread_id: thread.id.clone(),
            turn_id: None,
            ordinal: 0,
            status: ItemStatus::InProgress,
            body: ItemBody::AssistantMessage { text: text.into() },
            updated_at_ms: 0,
            parent_id: None,
            started_at_ms: None,
        };
        let id = item.id.clone();
        let m = ThreadModel::from_snapshot(Snapshot::Thread {
            seq: 5,
            thread,
            turns: vec![],
            items: vec![item],
            requests: vec![],
            queue: vec![],
        })
        .unwrap();
        (m, id)
    }

    fn delta(m: &ThreadModel, id: &ItemId, offset: usize, text: &str) -> Delta {
        Delta {
            thread_id: m.thread.id.clone(),
            item_id: id.clone(),
            channel: DeltaChannel::Text,
            offset,
            text: text.into(),
        }
    }

    fn text(m: &ThreadModel) -> String {
        match &m.items[0].body {
            ItemBody::AssistantMessage { text } => text.clone(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn deltas_dedupe_against_snapshot() {
        let (mut m, id) = model_with_item("hello ");
        // Fully contained → ignored.
        assert!(!m.apply_delta(&delta(&m, &id, 0, "hello ")));
        // Overlapping → only the new suffix is applied.
        assert!(m.apply_delta(&delta(&m, &id, 3, "lo world")));
        assert_eq!(text(&m), "hello world");
        // Contiguous.
        assert!(m.apply_delta(&delta(&m, &id, 11, "!")));
        // Gap → ignored.
        assert!(!m.apply_delta(&delta(&m, &id, 50, "?")));
        assert_eq!(text(&m), "hello world!");
    }

    #[test]
    fn stale_events_are_ignored() {
        let (mut m, _) = model_with_item("");
        let env = EventEnvelope {
            seq: 3,
            at_ms: 0,
            event: Event::ThreadUpdated {
                thread: m.thread.clone(),
            },
        };
        assert!(!m.apply_event(&env));
    }
}
