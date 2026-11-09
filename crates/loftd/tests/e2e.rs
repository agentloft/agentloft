//! End to end: real daemon on a real Unix socket, driven through
//! `loft-client`, with the scripted fake harness (PLAN M0 criterion 10).

use loft_client::model::{ThreadModel, WorkspaceModel};
use loft_client::{Client, ClientEvent};
use loft_config::Paths;
use loft_harness::Harness;
use loft_protocol::*;
use loftd::{Daemon, DaemonOptions};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(paths: &Paths) -> Daemon {
    let harnesses: Vec<Arc<dyn Harness>> = vec![Arc::new(loft_harness::fake::FakeHarness)];
    Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses,
        capture_login_env: false,
        mcp_bridge: None,
    })
    .await
    .unwrap()
}

async fn connect(paths: &Paths) -> (Client, UnboundedReceiver<ClientEvent>) {
    Client::connect(&paths.socket_file(), ClientKind::Test, "test")
        .await
        .unwrap()
}

async fn next(rx: &mut UnboundedReceiver<ClientEvent>) -> ClientEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for daemon")
        .expect("connection closed")
}

#[tokio::test]
async fn full_turn_over_socket_with_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let project_dir = dir.path().join("repo");
    std::fs::create_dir_all(&project_dir).unwrap();

    let daemon = start(&paths).await;

    // A second daemon on the same home must refuse to start.
    let second = Daemon::start(DaemonOptions {
        paths: paths.clone(),
        harnesses: vec![],
        capture_login_env: false,
        mcp_bridge: None,
    })
    .await;
    assert!(second.is_err(), "single-instance lock");

    let (client, mut rx) = connect(&paths).await;
    assert_eq!(client.request(Request::Ping).await.unwrap(), Response::Pong);

    // Workspace subscription sees the project and thread appear.
    let ws_sub = client.subscribe(Topic::Workspace);
    let mut workspace = WorkspaceModel::default();
    match next(&mut rx).await {
        ClientEvent::Snapshot { sub_id, snapshot } if sub_id == ws_sub => {
            workspace.apply_snapshot(snapshot)
        }
        other => panic!("{other:?}"),
    }
    let Response::Project { project } = client
        .request(Request::AddProject {
            path: project_dir.to_string_lossy().into_owned(),
            name: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let Response::Thread { thread } = client
        .request(Request::CreateThread {
            project_id: project.id.clone(),
            harness: HarnessId(HarnessId::FAKE.into()),
            title: None,
        })
        .await
        .unwrap()
    else {
        panic!()
    };

    let th_sub = client.subscribe(Topic::Thread(thread.id.clone()));
    let mut model: Option<ThreadModel> = None;
    client
        .request(Request::SendMessage {
            thread_id: thread.id.clone(),
            text: "hello over the socket".into(),
        })
        .await
        .unwrap();

    let mut deltas = 0;
    loop {
        match next(&mut rx).await {
            ClientEvent::Snapshot { sub_id, snapshot } if sub_id == th_sub => {
                model = ThreadModel::from_snapshot(snapshot);
            }
            ClientEvent::Event { sub_id, event } if sub_id == ws_sub => {
                workspace.apply_event(&event);
            }
            ClientEvent::Event { sub_id, event } if sub_id == th_sub => {
                model.as_mut().unwrap().apply_event(&event);
            }
            ClientEvent::Delta { sub_id, delta } if sub_id == th_sub => {
                if model.as_mut().unwrap().apply_delta(&delta) {
                    deltas += 1;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
        if let Some(m) = &model
            && m.last_turn().is_some_and(|t| t.status.is_terminal())
        {
            break;
        }
    }
    let m = model.unwrap();
    assert!(deltas > 0, "received streaming deltas");
    let reply = m
        .items
        .iter()
        .find_map(|i| match &i.body {
            ItemBody::AssistantMessage { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(reply, "echo: hello over the socket");
    assert_eq!(workspace.projects.len(), 1);
    assert_eq!(workspace.threads.len(), 1);

    // Disconnect, reconnect: snapshot reflects everything.
    drop(client);
    drop(rx);
    let (client, mut rx) = connect(&paths).await;
    client.subscribe(Topic::Thread(thread.id.clone()));
    let ClientEvent::Snapshot { snapshot, .. } = next(&mut rx).await else {
        panic!()
    };
    let m2 = ThreadModel::from_snapshot(snapshot).unwrap();
    assert_eq!(m2.items.len(), m.items.len());
    assert_eq!(m2.turns[0].status, TurnStatus::Completed);

    // Shutdown request stops the daemon and removes the socket.
    client
        .request(Request::Shutdown {
            reason: ShutdownReason::User,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), daemon.wait_for_shutdown_request())
        .await
        .unwrap();
    daemon.shutdown().await;
    assert!(!paths.socket_file().exists());

    // A fresh daemon on the same home recovers the data.
    let daemon = start(&paths).await;
    let (client, mut rx) = connect(&paths).await;
    client.subscribe(Topic::Workspace);
    let ClientEvent::Snapshot { snapshot, .. } = next(&mut rx).await else {
        panic!()
    };
    let mut ws = WorkspaceModel::default();
    ws.apply_snapshot(snapshot);
    assert_eq!(ws.threads[0].title, "hello over the socket");
    daemon.shutdown().await;
}

#[tokio::test]
async fn incompatible_protocol_is_refused() {
    use futures::{SinkExt as _, StreamExt as _};
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    let daemon = start(&paths).await;
    let stream = tokio::net::UnixStream::connect(paths.socket_file())
        .await
        .unwrap();
    let mut framed =
        tokio_util::codec::Framed::new(stream, tokio_util::codec::LengthDelimitedCodec::new());
    let hello = ClientMsg::Hello(Hello {
        protocol_version: PROTOCOL_VERSION + 5,
        client_kind: ClientKind::Test,
        client_build: "future".into(),
        capabilities: vec![],
    });
    framed
        .send(bytes::Bytes::from(serde_json::to_vec(&hello).unwrap()))
        .await
        .unwrap();
    let frame = framed.next().await.unwrap().unwrap();
    let msg: ServerMsg = serde_json::from_slice(&frame).unwrap();
    assert!(matches!(msg, ServerMsg::Fatal(e) if e.code == ErrorCode::IncompatibleVersion));
    daemon.shutdown().await;
}
