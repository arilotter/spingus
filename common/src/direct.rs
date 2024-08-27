use asynchronous_codec::{Framed, LengthCodec};
use futures::future::BoxFuture;
use futures::{AsyncRead, AsyncWrite, SinkExt, StreamExt};
use libp2p::core::{Multiaddr, UpgradeInfo};
use libp2p::swarm::{
    ConnectionHandler, ConnectionHandlerEvent, ConnectionId, FromSwarm, KeepAlive,
    NetworkBehaviour, NotifyHandler, PollParameters, SubstreamProtocol, THandlerInEvent, ToSwarm,
};
use libp2p::{InboundUpgrade, OutboundUpgrade, PeerId};
use log::{error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::task::{Context, Poll};

// Define the events that can be emitted by this behavior
#[derive(Debug)]
pub enum DirectMessageEvent {
    MessageSent { to: PeerId },
    MessageReceived { from: PeerId, message: Vec<u8> },
}

// Define the events that can be sent to the handler
#[derive(Debug)]
pub enum DirectMessageHandlerIn {
    SendMessage(Vec<u8>),
}

// Define the events that can be received from the handler
#[derive(Debug)]
pub enum DirectMessageHandlerOut {
    MessageReceived(Vec<u8>),
}

pub struct DirectMessage {
    events: VecDeque<ToSwarm<DirectMessageEvent, DirectMessageHandlerIn>>,
    pending_messages: HashMap<PeerId, VecDeque<Vec<u8>>>,
    connected_peers: HashMap<PeerId, ConnectionId>,
}

impl DirectMessage {
    pub fn new() -> Self {
        DirectMessage {
            events: VecDeque::new(),
            pending_messages: HashMap::new(),
            connected_peers: HashMap::new(),
        }
    }

    pub fn send_message(&mut self, to: PeerId, message: Vec<u8>) {
        self.pending_messages
            .entry(to)
            .or_default()
            .push_back(message);
    }
}

impl NetworkBehaviour for DirectMessage {
    type ConnectionHandler = DirectMessageHandler;
    type ToSwarm = DirectMessageEvent;

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        _local_addr: &Multiaddr,
        _remote_addr: &Multiaddr,
    ) -> Result<Self::ConnectionHandler, libp2p::swarm::ConnectionDenied> {
        self.connected_peers.insert(peer, connection_id);
        Ok(DirectMessageHandler::new())
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        _addr: &Multiaddr,
        _role_override: libp2p::core::Endpoint,
    ) -> Result<Self::ConnectionHandler, libp2p::swarm::ConnectionDenied> {
        self.connected_peers.insert(peer, connection_id);
        Ok(DirectMessageHandler::new())
    }

    fn on_swarm_event(&mut self, event: FromSwarm<Self::ConnectionHandler>) {
        if let FromSwarm::ConnectionClosed(closed) = event {
            self.connected_peers.remove(&closed.peer_id);
        }
    }

    fn on_connection_handler_event(
        &mut self,
        peer_id: PeerId,
        _connection_id: ConnectionId,
        event: <Self::ConnectionHandler as ConnectionHandler>::ToBehaviour,
    ) {
        match event {
            DirectMessageHandlerOut::MessageReceived(message) => {
                self.events.push_back(ToSwarm::GenerateEvent(
                    DirectMessageEvent::MessageReceived {
                        from: peer_id,
                        message,
                    },
                ));
            }
        }
    }

    fn poll(
        &mut self,
        _cx: &mut Context<'_>,
        _params: &mut impl PollParameters,
    ) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(event);
        }

        let peers: Vec<_> = self.pending_messages.keys().copied().collect();

        for peer_id in peers {
            if let Some(connection_id) = self.connected_peers.get(&peer_id) {
                let messages = self.pending_messages.get_mut(&peer_id);
                if let Some(messages) = messages {
                    if let Some(message) = messages.pop_front() {
                        let event = ToSwarm::NotifyHandler {
                            peer_id,
                            handler: NotifyHandler::One(*connection_id),
                            event: DirectMessageHandlerIn::SendMessage(message),
                        };
                        self.events.push_back(ToSwarm::GenerateEvent(
                            DirectMessageEvent::MessageSent { to: peer_id },
                        ));
                        return Poll::Ready(event);
                    }
                }
            } else {
                warn!("peer {peer_id} is not connected, dropping all messages");
                self.pending_messages.remove(&peer_id);
            }
        }

        Poll::Pending
    }
}

// Protocol definition
#[derive(Debug, Clone)]
pub struct DirectMessageProtocol;

impl UpgradeInfo for DirectMessageProtocol {
    type Info = &'static str;
    type InfoIter = std::iter::Once<Self::Info>;

    fn protocol_info(&self) -> Self::InfoIter {
        std::iter::once("/direct-message/1.0.0")
    }
}

impl<C> InboundUpgrade<C> for DirectMessageProtocol
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Output = Framed<C, LengthCodec>;
    type Error = io::Error;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, socket: C, _: Self::Info) -> Self::Future {
        Box::pin(async move {
            let framed = Framed::new(socket, LengthCodec);
            Ok(framed)
        })
    }
}

impl<C> OutboundUpgrade<C> for DirectMessageProtocol
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Output = Framed<C, LengthCodec>;
    type Error = io::Error;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, socket: C, _: Self::Info) -> Self::Future {
        Box::pin(async move {
            let framed = Framed::new(socket, LengthCodec);
            Ok(framed)
        })
    }
}

pub struct DirectMessageHandler {
    inbound_stream: Option<Framed<libp2p::Stream, LengthCodec>>,
    outbound_stream: Option<Framed<libp2p::Stream, LengthCodec>>,
    pending_events: VecDeque<
        ConnectionHandlerEvent<
            DirectMessageProtocol,
            (),
            DirectMessageHandlerOut,
            ConnectionHandlerError,
        >,
    >,
}

impl DirectMessageHandler {
    fn new() -> Self {
        DirectMessageHandler {
            inbound_stream: None,
            outbound_stream: None,
            pending_events: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
pub struct ConnectionHandlerError;
impl std::fmt::Display for ConnectionHandlerError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}
impl std::error::Error for ConnectionHandlerError {}

impl ConnectionHandler for DirectMessageHandler {
    type FromBehaviour = DirectMessageHandlerIn;
    type ToBehaviour = DirectMessageHandlerOut;
    type InboundProtocol = DirectMessageProtocol;
    type OutboundProtocol = DirectMessageProtocol;
    type InboundOpenInfo = ();
    type OutboundOpenInfo = ();
    type Error = ConnectionHandlerError;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(DirectMessageProtocol, ())
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::ToBehaviour,
            Self::Error,
        >,
    > {
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(event);
        }

        if let Some(stream) = self.inbound_stream.as_mut() {
            match stream.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(message))) => {
                    return Poll::Ready(ConnectionHandlerEvent::NotifyBehaviour(
                        DirectMessageHandlerOut::MessageReceived(message.to_vec()),
                    ));
                }
                Poll::Ready(None) | Poll::Ready(Some(Err(_))) => {
                    self.inbound_stream = None;
                }
                Poll::Pending => {}
            }
        }

        Poll::Pending
    }

    fn on_behaviour_event(&mut self, event: Self::FromBehaviour) {
        match event {
            DirectMessageHandlerIn::SendMessage(message) => {
                if let Some(stream) = self.outbound_stream.as_mut() {
                    let fut = stream.send(message.into());
                    self.pending_events.push_back(
                        ConnectionHandlerEvent::OutboundSubstreamRequest {
                            protocol: SubstreamProtocol::new(DirectMessageProtocol, ()),
                        },
                    );
                    // In a real implementation, you'd want to properly handle the future
                    // and any potential errors. This is a simplified version.
                    let _ = futures::executor::block_on(fut);
                }
            }
        }
    }

    fn on_connection_event(
        &mut self,
        event: libp2p::swarm::handler::ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            libp2p::swarm::handler::ConnectionEvent::FullyNegotiatedInbound(stream) => {
                self.inbound_stream = Some(stream.protocol);
            }
            libp2p::swarm::handler::ConnectionEvent::FullyNegotiatedOutbound(stream) => {
                self.outbound_stream = Some(stream.protocol);
            }
            _ => {}
        }
    }
}
