use crate::codec::*;
use crate::fair_queue::FairQueue;
use crate::message::*;
use crate::util::*;
use crate::{util, BlockingRecv, MultiPeer, Socket, SocketBackend, SocketType, ZmqResult};
use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use dashmap::DashMap;
use futures::channel::{mpsc, oneshot};
use futures::SinkExt;
use futures::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;

pub(crate) struct SubPeer {
    pub(crate) _identity: PeerIdentity,
    pub(crate) send_queue: mpsc::Sender<Message>,
    pub(crate) recv_queue_in: mpsc::Sender<Message>,
    pub(crate) _io_close_handle: futures::channel::oneshot::Sender<bool>,
}

pub(crate) struct SubSocketBackend {
    pub(crate) peers: DashMap<PeerIdentity, SubPeer>,
    pub(crate) peer_queue_in: mpsc::Sender<(PeerIdentity, mpsc::Receiver<Message>)>,
}

pub struct SubSocket {
    backend: Arc<SubSocketBackend>,
    fair_queue: mpsc::Receiver<(PeerIdentity, Message)>,
    _accept_close_handle: Option<oneshot::Sender<bool>>,
    _fair_queue_close_handle: oneshot::Sender<bool>,
}

impl Drop for SubSocket {
    fn drop(&mut self) {
        self.backend.shutdown()
    }
}

#[async_trait]
impl SocketBackend for SubSocketBackend {
    async fn message_received(&self, peer_id: &PeerIdentity, message: Message) {
        self.peers
            .get_mut(peer_id)
            .expect("Not found peer by id")
            .recv_queue_in
            .send(message)
            .await
            .expect("Failed to send");
    }

    fn socket_type(&self) -> SocketType {
        SocketType::SUB
    }

    fn shutdown(&self) {
        self.peers.clear();
    }
}

#[async_trait]
impl MultiPeer for SubSocketBackend {
    async fn peer_connected(
        &self,
        peer_id: &PeerIdentity,
    ) -> (mpsc::Receiver<Message>, oneshot::Receiver<bool>) {
        let default_queue_size = 100;
        let (out_queue, out_queue_receiver) = mpsc::channel(1);
        let (in_queue, in_queue_receiver) = mpsc::channel::<Message>(default_queue_size);
        let (stop_handle, stop_callback) = oneshot::channel::<bool>();

        self.peers.insert(
            peer_id.clone(),
            SubPeer {
                _identity: peer_id.clone(),
                send_queue: out_queue,
                recv_queue_in: in_queue,
                _io_close_handle: stop_handle,
            },
        );
        self.peer_queue_in
            .clone()
            .try_send((peer_id.clone(), in_queue_receiver))
            .unwrap();

        (out_queue_receiver, stop_callback)
    }

    async fn peer_disconnected(&self, peer_id: &PeerIdentity) {
        self.peers.remove(peer_id);
    }
}

impl SubSocket {
    pub async fn subscribe(&mut self, subscription: &str) -> ZmqResult<()> {
        let mut message = BytesMut::with_capacity(subscription.len() + 1);
        message.put_u8(1);
        message.extend_from_slice(subscription.as_bytes());
        //let message = format!("\0x1{}", subscription);
        for mut peer in self.backend.peers.iter_mut() {
            peer.send_queue
                .send(Message::Message(message.clone().into()))
                .await?;
        }
        Ok(())
    }

    pub async fn unsubscribe(&mut self, subscription: &str) -> ZmqResult<()> {
        let mut message = BytesMut::with_capacity(subscription.len() + 1);
        message.put_u8(0);
        message.extend_from_slice(subscription.as_bytes());
        for mut peer in self.backend.peers.iter_mut() {
            peer.send_queue
                .send(Message::Message(message.clone().into()))
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Socket for SubSocket {
    fn new() -> Self {
        // TODO define buffer size
        let default_queue_size = 100;
        let (queue_sender, fair_queue) = mpsc::channel(default_queue_size);
        let (peer_in, peer_out) = mpsc::channel(default_queue_size);
        let (fair_queue_close_handle, fqueue_close_recevier) = oneshot::channel();
        tokio::spawn(util::process_fair_queue_messages(FairQueueProcessor {
            fair_queue_stream: FairQueue::new(),
            socket_incoming_queue: queue_sender,
            peer_queue_in: peer_out,
            _io_close_handle: fqueue_close_recevier,
        }));
        Self {
            backend: Arc::new(SubSocketBackend {
                peers: Default::default(),
                peer_queue_in: peer_in,
            }),
            fair_queue,
            _accept_close_handle: None,
            _fair_queue_close_handle: fair_queue_close_handle,
        }
    }

    async fn bind(&mut self, endpoint: &str) -> ZmqResult<()> {
        let stop_handle = util::start_accepting_connections(endpoint, self.backend.clone()).await?;
        self._accept_close_handle = Some(stop_handle);
        Ok(())
    }

    async fn connect(&mut self, endpoint: &str) -> ZmqResult<()> {
        let addr = endpoint.parse::<SocketAddr>()?;
        let raw_socket = tokio::net::TcpStream::connect(addr).await?;
        util::peer_connected(raw_socket, self.backend.clone()).await;
        Ok(())
    }
}

#[async_trait]
impl BlockingRecv for SubSocket {
    async fn recv(&mut self) -> ZmqResult<ZmqMessage> {
        loop {
            match self.fair_queue.next().await {
                Some((_peer_id, Message::Message(message))) => {
                    return Ok(message);
                }
                Some((_peer_id, _)) => todo!(),
                None => todo!(),
            }
        }
    }
}
