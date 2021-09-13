use crate::{
    assets::Assets,
    channel::ChannelList,
    fs::RealFs,
    language::LanguageRegistry,
    rpc::{self, Client},
    settings::{self, ThemeRegistry},
    time::ReplicaId,
    user::UserStore,
    AppState,
};
use anyhow::{anyhow, Result};
use gpui::{AsyncAppContext, Entity, ModelHandle, MutableAppContext, TestAppContext};
use parking_lot::Mutex;
use postage::{mpsc, prelude::Stream as _, sink::Sink as _, watch};
use smol::channel;
use std::{
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering::SeqCst},
        Arc,
    },
};
use tempdir::TempDir;
use zrpc::{proto, Conn, ConnectionId, Peer, Receipt, TypedEnvelope};

#[cfg(test)]
#[ctor::ctor]
fn init_logger() {
    env_logger::init();
}

#[derive(Clone)]
struct Envelope<T: Clone> {
    message: T,
    sender: ReplicaId,
}

#[cfg(test)]
pub(crate) struct Network<T: Clone, R: rand::Rng> {
    inboxes: std::collections::BTreeMap<ReplicaId, Vec<Envelope<T>>>,
    all_messages: Vec<T>,
    rng: R,
}

#[cfg(test)]
impl<T: Clone, R: rand::Rng> Network<T, R> {
    pub fn new(rng: R) -> Self {
        Network {
            inboxes: Default::default(),
            all_messages: Vec::new(),
            rng,
        }
    }

    pub fn add_peer(&mut self, id: ReplicaId) {
        self.inboxes.insert(id, Vec::new());
    }

    pub fn is_idle(&self) -> bool {
        self.inboxes.values().all(|i| i.is_empty())
    }

    pub fn broadcast(&mut self, sender: ReplicaId, messages: Vec<T>) {
        for (replica, inbox) in self.inboxes.iter_mut() {
            if *replica != sender {
                for message in &messages {
                    let min_index = inbox
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(index, envelope)| {
                            if sender == envelope.sender {
                                Some(index + 1)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);

                    // Insert one or more duplicates of this message *after* the previous
                    // message delivered by this replica.
                    for _ in 0..self.rng.gen_range(1..4) {
                        let insertion_index = self.rng.gen_range(min_index..inbox.len() + 1);
                        inbox.insert(
                            insertion_index,
                            Envelope {
                                message: message.clone(),
                                sender,
                            },
                        );
                    }
                }
            }
        }
        self.all_messages.extend(messages);
    }

    pub fn has_unreceived(&self, receiver: ReplicaId) -> bool {
        !self.inboxes[&receiver].is_empty()
    }

    pub fn receive(&mut self, receiver: ReplicaId) -> Vec<T> {
        let inbox = self.inboxes.get_mut(&receiver).unwrap();
        let count = self.rng.gen_range(0..inbox.len() + 1);
        inbox
            .drain(0..count)
            .map(|envelope| envelope.message)
            .collect()
    }
}

pub fn sample_text(rows: usize, cols: usize) -> String {
    let mut text = String::new();
    for row in 0..rows {
        let c: char = ('a' as u32 + row as u32) as u8 as char;
        let mut line = c.to_string().repeat(cols);
        if row < rows - 1 {
            line.push('\n');
        }
        text += &line;
    }
    text
}

pub fn temp_tree(tree: serde_json::Value) -> TempDir {
    let dir = TempDir::new("").unwrap();
    write_tree(dir.path(), tree);
    dir
}

fn write_tree(path: &Path, tree: serde_json::Value) {
    use serde_json::Value;
    use std::fs;

    if let Value::Object(map) = tree {
        for (name, contents) in map {
            let mut path = PathBuf::from(path);
            path.push(name);
            match contents {
                Value::Object(_) => {
                    fs::create_dir(&path).unwrap();
                    write_tree(&path, contents);
                }
                Value::Null => {
                    fs::create_dir(&path).unwrap();
                }
                Value::String(contents) => {
                    fs::write(&path, contents).unwrap();
                }
                _ => {
                    panic!("JSON object must contain only objects, strings, or null");
                }
            }
        }
    } else {
        panic!("You must pass a JSON object to this helper")
    }
}

pub fn test_app_state(cx: &mut MutableAppContext) -> Arc<AppState> {
    let (settings_tx, settings) = settings::test(cx);
    let languages = Arc::new(LanguageRegistry::new());
    let themes = ThemeRegistry::new(Assets, cx.font_cache().clone());
    let rpc = rpc::Client::new();
    let user_store = Arc::new(UserStore::new(rpc.clone()));
    Arc::new(AppState {
        settings_tx: Arc::new(Mutex::new(settings_tx)),
        settings,
        themes,
        languages: languages.clone(),
        channel_list: cx.add_model(|cx| ChannelList::new(user_store, rpc.clone(), cx)),
        rpc,
        fs: Arc::new(RealFs),
    })
}

pub struct Observer<T>(PhantomData<T>);

impl<T: 'static> Entity for Observer<T> {
    type Event = ();
}

impl<T: Entity> Observer<T> {
    pub fn new(
        handle: &ModelHandle<T>,
        cx: &mut gpui::TestAppContext,
    ) -> (ModelHandle<Self>, channel::Receiver<()>) {
        let (notify_tx, notify_rx) = channel::unbounded();
        let observer = cx.add_model(|cx| {
            cx.observe(handle, move |_, _, _| {
                let _ = notify_tx.try_send(());
            })
            .detach();
            Observer(PhantomData)
        });
        (observer, notify_rx)
    }
}

pub struct FakeServer {
    peer: Arc<Peer>,
    connection: Mutex<Option<Connection>>,
    forbid_new_connections: AtomicBool,
    forbid_reconnections: AtomicBool,
}

struct Connection {
    id: ConnectionId,
    incoming: mpsc::Receiver<Box<dyn proto::AnyTypedEnvelope>>,
    token: u128,
    kill_tx: watch::Sender<Option<()>>,
}

impl FakeServer {
    pub async fn for_client(
        client_user_id: u64,
        client: &mut Arc<Client>,
        cx: &TestAppContext,
    ) -> Arc<Self> {
        let result = Arc::new(Self {
            peer: Peer::new(),
            connection: Default::default(),
            forbid_new_connections: Default::default(),
            forbid_reconnections: Default::default(),
        });

        Arc::get_mut(client)
            .unwrap()
            .set_login_and_connect_callbacks(
                move |cx| {
                    cx.spawn(|_| async move {
                        let access_token = "the-token".to_string();
                        Ok((client_user_id, access_token))
                    })
                },
                {
                    let server = result.clone();
                    move |opts, cx| {
                        assert_eq!(opts.user_id, client_user_id);
                        assert_eq!(opts.access_token, "the-token");
                        cx.spawn({
                            let server = server.clone();
                            let opts = opts.clone();
                            move |cx| async move { server.connect(opts, &cx).await }
                        })
                    }
                },
            );

        client
            .authenticate_and_connect(&cx.to_async())
            .await
            .unwrap();
        result
    }

    pub async fn disconnect(&self) {
        self.peer.disconnect(self.connection_id()).await;
        self.connection.lock().take();
    }

    pub async fn kill_connection(&self) {
        let mut connection = self.connection.lock();
        let connection = connection.as_mut().expect("not connected");
        let _ = connection.kill_tx.send(Some(())).await;
    }

    async fn connect(&self, opts: rpc::ConnectionOptions, cx: &AsyncAppContext) -> Result<Conn> {
        if opts.is_reconnection {
            if self.forbid_reconnections.load(SeqCst) {
                Err(anyhow!("server is forbidding reconnections"))
            } else {
                let mut connection = self.connection.lock();
                if connection
                    .as_ref()
                    .map_or(false, |c| c.token == opts.connection_token)
                {
                    let connection = connection.as_mut().unwrap();
                    let (client_conn, server_conn, kill_tx) = Conn::in_memory();
                    let io = self.peer.reconnect(connection.id, server_conn).await?;
                    connection.kill_tx = kill_tx;
                    cx.background().spawn(io).detach();
                    Ok(client_conn)
                } else {
                    Err(anyhow!("cannot re-establish connection"))
                }
            }
        } else {
            if self.forbid_new_connections.load(SeqCst) {
                Err(anyhow!("server is forbidding connections"))
            } else {
                let (client_conn, server_conn, kill_tx) = Conn::in_memory();
                let (connection_id, io, incoming) = self.peer.connect(server_conn).await;
                cx.background().spawn(io).detach();
                *self.connection.lock() = Some(Connection {
                    id: connection_id,
                    incoming,
                    token: opts.connection_token,
                    kill_tx,
                });
                Ok(client_conn)
            }
        }
    }

    pub fn forbid_new_connections(&self) {
        self.forbid_new_connections.store(true, SeqCst);
    }

    pub fn allow_new_connections(&self) {
        self.forbid_new_connections.store(false, SeqCst);
    }

    pub fn forbid_reconnections(&self) {
        self.forbid_reconnections.store(true, SeqCst);
    }

    pub fn allow_reconnections(&self) {
        self.forbid_reconnections.store(false, SeqCst);
    }

    pub async fn send<T: proto::EnvelopedMessage>(&self, message: T) {
        self.peer.send(self.connection_id(), message).await.unwrap();
    }

    pub async fn receive<M: proto::EnvelopedMessage>(&self) -> Result<TypedEnvelope<M>> {
        let mut connection = self.connection.lock();
        let message = connection
            .as_mut()
            .expect("not connected")
            .incoming
            .recv()
            .await
            .ok_or_else(|| anyhow!("other half hung up"))?;
        let type_name = message.payload_type_name();
        Ok(*message
            .into_any()
            .downcast::<TypedEnvelope<M>>()
            .unwrap_or_else(|_| {
                panic!(
                    "fake server received unexpected message type: {:?}",
                    type_name
                );
            }))
    }

    pub async fn respond<T: proto::RequestMessage>(
        &self,
        receipt: Receipt<T>,
        response: T::Response,
    ) {
        self.peer.respond(receipt, response).await.unwrap()
    }

    fn connection_id(&self) -> ConnectionId {
        self.connection.lock().as_ref().expect("not connected").id
    }
}
