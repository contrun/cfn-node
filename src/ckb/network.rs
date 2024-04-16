use log::{debug, error, info, warn};
use ractor::{async_trait as rasync_trait, Actor, ActorCell, ActorProcessingErr, ActorRef};
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr, FromInto};
use std::{collections::HashMap, str};
use tentacle::context::SessionContext;
use tentacle::{multiaddr::Multiaddr, secio::PeerId, SessionId};

use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId,
};

use tokio_util::task::TaskTracker;

use super::peer::get_peer_actor_name;
use super::{
    channel::ChannelCommand,
    channel::{ChannelActor, ChannelInitializationParameter},
    peer::PeerActor,
    peer::PeerActorMessage,
    types::PCNMessage,
    CkbConfig,
};

use crate::events::{Event, EventActorMessage};
use crate::unwrap_or_return;

pub const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

#[derive(Clone, Debug, Deserialize)]
pub struct NetworkRequest {
    pub id: u64,
    pub request: NetworkActorCommand,
}

#[derive(Clone, Debug)]
pub struct NetworkResponse {
    pub id: u64,
    pub response: NetworkActorEvent,
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub enum NetworkActorCommand {
    /// Network commands
    ConnectPeer(Multiaddr),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendPcnMessage(PCNMessageWithPeerId),
    // Directly send a message to session
    SendPcnMessageToSession(PCNMessageWithSessionId),
    ControlPcnChannel(ChannelCommand),
}

impl NetworkActorMessage {
    pub fn new_command(command: NetworkActorCommand) -> Self {
        Self::Command(command)
    }

    pub fn new_request(id: NetworkRequestId, request: NetworkActorCommand) -> Self {
        Self::Request(id, request)
    }

    pub fn new_event(event: NetworkActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_response(id: NetworkRequestId, response: NetworkActorEvent) -> Self {
        Self::Response(id, response)
    }
}

#[derive(Clone, Debug)]
pub enum NetworkServiceEvent {
    ServiceError(String),
    ServiceEvent(String),
    PeerConnected(Multiaddr),
    PeerDisConnected(Multiaddr),
}

#[derive(Clone, Debug)]
pub enum NetworkActorEvent {
    /// Network eventss to be processed by this actor.
    PeerConnected(PeerId, SessionContext),
    PeerDisconnected(PeerId, SessionContext),
    PeerMessage(PeerId, SessionContext, PCNMessage),

    /// Network service events to be sent to outside observers.
    NetworkServiceEvent(NetworkServiceEvent),
}

pub type NetworkRequestId = u64;

#[derive(Debug)]
pub enum NetworkActorMessage {
    Command(NetworkActorCommand),
    Event(NetworkActorEvent),
    Request(NetworkRequestId, NetworkActorCommand),
    Response(NetworkRequestId, NetworkActorEvent),
}

enum InternalNetworkActorMessage {
    Command(Option<NetworkRequestId>, NetworkActorCommand),
    Event(Option<NetworkRequestId>, NetworkActorEvent),
}

impl From<NetworkActorMessage> for InternalNetworkActorMessage {
    fn from(message: NetworkActorMessage) -> Self {
        match message {
            NetworkActorMessage::Command(command) => {
                InternalNetworkActorMessage::Command(None, command)
            }
            NetworkActorMessage::Event(event) => InternalNetworkActorMessage::Event(None, event),
            NetworkActorMessage::Request(id, request) => {
                InternalNetworkActorMessage::Command(Some(id), request)
            }
            NetworkActorMessage::Response(id, response) => {
                InternalNetworkActorMessage::Event(Some(id), response)
            }
        }
    }
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithPeerId {
    #[serde_as(as = "DisplayFromStr")]
    pub peer_id: PeerId,
    pub message: PCNMessage,
}

#[serde_as]
#[derive(Clone, Debug, Deserialize)]
pub struct PCNMessageWithSessionId {
    #[serde_as(as = "FromInto<usize>")]
    pub session_id: SessionId,
    pub message: PCNMessage,
}

pub struct NetworkActor {
    event_actor: ActorRef<EventActorMessage>,
}

impl NetworkActor {
    pub async fn emit_event_or_response(
        &self,
        id: Option<NetworkRequestId>,
        event: NetworkServiceEvent,
    ) {
        match id {
            Some(id) => self.emit_response(id, event).await,
            None => self.emit_event(event).await,
        }
    }

    pub async fn emit_event(&self, event: NetworkServiceEvent) {}

    pub async fn emit_response(&self, id: NetworkRequestId, event: NetworkServiceEvent) {
        let _ = self
            .event_actor
            .send_message(EventActorMessage::ProcessEvent(Event::NetworkResponse(
                NetworkResponse {
                    id,
                    response: NetworkActorEvent::NetworkServiceEvent(event),
                },
            )))
            .expect("event actor alive");
    }
}

pub struct NetworkActorState {
    peer_id: PeerId,
    // This immutable attribute is placed here because we need to create it in
    // the pre_start function.
    control: ServiceAsyncControl,
    peers: HashMap<PeerId, ActorRef<PeerActorMessage>>,
}

impl NetworkActorState {
    /// Get or create a peer actor.
    pub async fn get_or_create_peer(
        &mut self,
        id: PeerId,
        control: &ActorRef<NetworkActorMessage>,
    ) -> ActorRef<PeerActorMessage> {
        match self.peers.get(&id) {
            Some(actor) => actor.clone(),
            None => {
                let peer_name = get_peer_actor_name(&id);
                let actor = Actor::spawn_linked(
                    Some(peer_name),
                    PeerActor::new(Some(id.clone()), control.clone()),
                    (),
                    control.get_cell(),
                )
                .await
                .expect("spawn peer actor")
                .0;
                self.peers.insert(id, actor.clone());
                actor
            }
        }
    }
}

#[rasync_trait]
impl Actor for NetworkActor {
    type Msg = NetworkActorMessage;
    type State = NetworkActorState;
    type Arguments = (CkbConfig, TaskTracker);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (config, tracker): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let pk = kp.public_key();
        let handle = Handle::new(myself.clone());
        let mut service = ServiceBuilder::default()
            .insert_protocol(handle.clone().create_meta(PCN_PROTOCOL_ID))
            .key_pair(kp)
            .build(handle);
        let listen_addr = service
            .listen(
                format!("/ip4/127.0.0.1/tcp/{}", config.listening_port)
                    .parse()
                    .expect("valid tentacle address"),
            )
            .await
            .expect("listen tentacle");

        let my_peer_id: PeerId = PeerId::from(pk);
        info!(
            "Started listening tentacle on {}/p2p/{}",
            listen_addr,
            my_peer_id.to_base58()
        );

        let control = service.control().to_owned();

        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service shutdown");
        });

        Ok(NetworkActorState {
            peer_id: my_peer_id,
            peers: HashMap::new(),
            control,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Network actor processing message {:?}", message);

        let message = InternalNetworkActorMessage::from(message);
        match message {
            InternalNetworkActorMessage::Event(request_id, event) => match event {
                NetworkActorEvent::NetworkServiceEvent(e) => {
                    self.emit_event_or_response(request_id, e).await;
                }

                NetworkActorEvent::PeerConnected(id, session) => match state.peers.get(&id) {
                    Some(_actor) => {
                        warn!("Duplicated peer connected event. Are we connecting here? The connection reestablishment processing is not implemented");
                    }
                    None => {
                        let peer_name = get_peer_actor_name(&id);
                        let actor = Actor::spawn_linked(
                            Some(peer_name),
                            PeerActor::new(Some(id.clone()), myself.clone()),
                            (),
                            myself.get_cell(),
                        )
                        .await
                        .expect("spawn peer actor")
                        .0;

                        self.emit_event_or_response(
                            request_id,
                            NetworkServiceEvent::PeerConnected(session.address.clone()),
                        )
                        .await;

                        actor
                            .send_message(PeerActorMessage::Connected(session))
                            .expect("peer actor alive");
                        state.peers.insert(id, actor.clone());
                    }
                },
                NetworkActorEvent::PeerDisconnected(id, session) => match state.peers.remove(&id) {
                    Some(actor) => {
                        debug!("Removed actor for peer {:?} from network actor", id);
                        self.emit_event_or_response(
                            request_id,
                            NetworkServiceEvent::PeerDisConnected(session.address.clone()),
                        )
                        .await;
                        actor
                            .send_message(PeerActorMessage::Disconnected(session))
                            .expect("peer actor alive");
                    }
                    None => {
                        warn!("Trying to remove a not found peer {:?}", &id);
                    }
                },
                NetworkActorEvent::PeerMessage(id, session, message) => {
                    match state.peers.get(&id) {
                        Some(actor) => {
                            actor
                                .send_message(PeerActorMessage::ReceivedMessage(session, message))
                                .expect("peer actor alive");
                        }
                        None => {
                            warn!("Received message for a not found peer {:?}", &id);
                        }
                    }
                }
            },
            InternalNetworkActorMessage::Command(_id, command) => match command {
                NetworkActorCommand::SendPcnMessageToSession(PCNMessageWithSessionId {
                    session_id,
                    message,
                }) => {
                    let result = state
                        .control
                        .send_message_to(session_id, PCN_PROTOCOL_ID, message.to_molecule_bytes())
                        .await;
                    if let Err(err) = result {
                        error!(
                            "Sending message to session {:?} failed: {}",
                            &session_id, err
                        );
                        return Ok(());
                    }
                    debug!("Message send to session {:?}", &session_id);
                }

                NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId { peer_id, message }) => {
                    match state.peers.get(&peer_id) {
                        Some(actor) => {
                            actor
                                .send_message(PeerActorMessage::SendMessage(message))
                                .expect("peer actor alive");
                        }
                        None => {
                            error!("Sending messages to a not found peer {:?}", &peer_id);
                        }
                    }
                }

                NetworkActorCommand::ConnectPeer(addr) => {
                    // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                    // e.g. whether the peer support some specific feature.
                    // TODO: If we are already connected to the peer, skip connecting.
                    debug!("Dialing {}", &addr);
                    let result = state.control.dial(addr.clone(), TargetProtocol::All).await;
                    if let Err(err) = result {
                        error!("Dialing {} failed: {}", &addr, err);
                    }
                }

                NetworkActorCommand::ControlPcnChannel(c) => match c {
                    ChannelCommand::OpenChannel(open_channel) => {
                        let peer_actor = state.peers.get(&open_channel.peer_id).cloned();
                        match peer_actor {
                            None => {
                                warn!(
                                    "Trying to control a not found peer {:?}",
                                    &open_channel.peer_id
                                );
                                return Ok(());
                            }
                            Some(peer_actor) => {
                                if let Err(err) = Actor::spawn_linked(
                                    Some("channel".to_string()),
                                    ChannelActor::new(myself.clone(), peer_actor),
                                    ChannelInitializationParameter::OpenChannelCommand(
                                        open_channel,
                                    ),
                                    myself.clone().get_cell(),
                                )
                                .await
                                {
                                    error!("Failed to start channel actor: {}", err);
                                }
                            }
                        }
                    }
                },
            },
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Err(err) = state.control.close().await {
            error!("Failed to close tentacle service: {}", err);
        }
        debug!("Tentacle service shutdown");
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Handle {
    actor: ActorRef<NetworkActorMessage>,
}

impl Handle {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        Self { actor }
    }

    async fn emit_event(&self, event: NetworkServiceEvent) {
        let _ = self
            .actor
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(event),
            ))
            .expect("network actor alive");
    }

    async fn emit_response(&self, id: NetworkRequestId, event: NetworkServiceEvent) {
        let _ = self
            .actor
            .send_message(NetworkActorMessage::new_response(
                id,
                NetworkActorEvent::NetworkServiceEvent(event),
            ))
            .expect("network actor alive");
    }
    fn create_meta(self, id: ProtocolId) -> ProtocolMeta {
        MetaBuilder::new()
            .id(id)
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }
}

#[async_trait]
impl ServiceProtocol for Handle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );

        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.actor
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerConnected(peer_id, context.session.clone()),
                ))
                .expect("network actor alive");
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );

        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.actor
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerDisconnected(peer_id, context.session.clone()),
                ))
                .expect("network actor alive");
        } else {
            warn!(
                "Peer disconnected without remote pubkey {:?}",
                context.session
            );
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        info!(
            "received from [{}]: proto [{}] data {:?}",
            context.session.id,
            context.proto_id,
            hex::encode(data.as_ref()),
        );

        let msg = unwrap_or_return!(PCNMessage::from_molecule_slice(&data), "parse message");
        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.actor
                .send_message(NetworkActorMessage::new_event(
                    NetworkActorEvent::PeerMessage(peer_id, context.session.clone(), msg),
                ))
                .expect("network actor alive");
        } else {
            warn!(
                "Received message from a peer without remote pubkey {:?}",
                context.session
            );
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[async_trait]
impl ServiceHandle for Handle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.emit_event(NetworkServiceEvent::ServiceError(format!(
            "Service error: {:?}",
            error
        )))
        .await;
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.emit_event(NetworkServiceEvent::ServiceEvent(format!(
            "Service event: {:?}",
            event
        )))
        .await;
    }
}

pub async fn start_ckb(
    config: CkbConfig,
    event_actor: ActorRef<EventActorMessage>,
    tracker: TaskTracker,
    supervisor: ActorCell,
) -> ActorRef<NetworkActorMessage> {
    let (actor, _handle) = Actor::spawn_linked(
        Some("network actor".to_string()),
        NetworkActor { event_actor },
        (config, tracker),
        supervisor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}
