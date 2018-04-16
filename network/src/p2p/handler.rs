// Copyright 2018 Kodebox, Inc.
// This file is part of CodeChain.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::collections::{HashMap, HashSet};
use std::convert::From;
use std::io;
use std::sync::Arc;

use cfinally::finally;
use cio::{IoContext, IoHandler, IoHandlerResult, IoManager, StreamToken, TimerToken};
use mio::deprecated::EventLoop;
use mio::{PollOpt, Ready, Token};
use parking_lot::{Mutex, RwLock};

use super::super::client::Client;
use super::super::extension::NodeToken;
use super::super::session::{Nonce, Session, SessionTable};
use super::super::token_generator::TokenGenerator;
use super::super::SocketAddr;
use super::connection::{Connection, ExtensionCallback as ExtensionChannel};
use super::listener::Listener;
use super::message::Version;
use super::stream::Stream;
use super::unprocessed_connection::UnprocessedConnection;

struct Manager {
    listener: Listener,

    tokens: TokenGenerator,
    unprocessed_tokens: HashSet<StreamToken>,
    connections: HashMap<StreamToken, Connection>,
    unprocessed_connections: HashMap<StreamToken, UnprocessedConnection>,

    registered_sessions: HashMap<Nonce, Session>,
    socket_to_session: SessionTable,

    waiting_sync_tokens: TokenGenerator,
    waiting_sync_stream_to_timer: HashMap<StreamToken, TimerToken>,
    waiting_sync_timer_to_stream: HashMap<TimerToken, StreamToken>,
}

const MAX_CONNECTIONS: usize = 32;

const ACCEPT_TOKEN: TimerToken = 0;

const FIRST_CONNECTION_TOKEN: TimerToken = ACCEPT_TOKEN + 1;
const LAST_CONNECTION_TOKEN: TimerToken = FIRST_CONNECTION_TOKEN + MAX_CONNECTIONS;

const FIRST_WAIT_SYNC_TOKEN: TimerToken = LAST_CONNECTION_TOKEN;
const MAX_SYNC_WAITS: usize = 10;
const LAST_WAIT_SYNC_TOKEN: TimerToken = FIRST_WAIT_SYNC_TOKEN + MAX_SYNC_WAITS;

const WAIT_SYNC_MS: u64 = 10 * 1000;

#[derive(Clone, Debug, PartialOrd, PartialEq)]
pub enum Message {
    RegisterSession(SocketAddr, Session),

    RequestConnection(SocketAddr, Session),

    RequestNegotiation {
        node_id: NodeToken,
        extension_name: String,
        version: Version,
    },
    SendExtensionMessage {
        node_id: NodeToken,
        extension_name: String,
        need_encryption: bool,
        data: Vec<u8>,
    },
}

#[derive(Debug)]
enum Error {
    InvalidStream(StreamToken),
    InvalidNode(NodeToken),
    General(&'static str),
}

type Result<T> = ::std::result::Result<T, Error>;

impl ::std::fmt::Display for Error {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        match self {
            &Error::InvalidStream(_) => ::std::fmt::Debug::fmt(self, f),
            &Error::InvalidNode(_) => ::std::fmt::Debug::fmt(self, f),
            &Error::General(_) => ::std::fmt::Debug::fmt(self, f),
        }
    }
}


impl Manager {
    pub fn listen(socket_address: &SocketAddr) -> io::Result<Self> {
        Ok(Manager {
            listener: Listener::bind(&socket_address)?,

            tokens: TokenGenerator::new(FIRST_CONNECTION_TOKEN, LAST_CONNECTION_TOKEN),
            unprocessed_tokens: HashSet::new(),
            connections: HashMap::new(),
            unprocessed_connections: HashMap::new(),

            registered_sessions: HashMap::new(),
            socket_to_session: SessionTable::new(),

            waiting_sync_tokens: TokenGenerator::new(FIRST_WAIT_SYNC_TOKEN, LAST_WAIT_SYNC_TOKEN),
            waiting_sync_stream_to_timer: HashMap::new(),
            waiting_sync_timer_to_stream: HashMap::new(),
        })
    }

    fn register_unprocessed_connection(&mut self, stream: Stream) -> Result<(StreamToken, TimerToken)> {
        let token = self.tokens.gen().ok_or(Error::General("TooManyConnections"))?;
        let timer_token = {
            if let Some(timer_token) = self.waiting_sync_tokens.gen() {
                timer_token
            } else {
                return Err(Error::General("TooManyWaitingSync"))
            }
        };

        let t = self.waiting_sync_stream_to_timer.insert(token, timer_token);
        debug_assert!(t.is_none());
        let t = self.waiting_sync_timer_to_stream.insert(token, timer_token);
        debug_assert!(t.is_none());

        let connection = UnprocessedConnection::new(stream);

        let con = self.unprocessed_connections.insert(token, connection);
        debug_assert!(con.is_none());

        let t = self.unprocessed_tokens.insert(token);
        debug_assert!(t);

        Ok((token, timer_token))
    }

    fn register_connection(&mut self, connection: Connection, token: &StreamToken) {
        let con = self.connections.insert(*token, connection);
        debug_assert!(con.is_none());
    }

    fn process_connection(&mut self, unprocessed_token: &StreamToken) -> Connection {
        let unprocessed = self.remove_waiting_sync_by_stream_token(&unprocessed_token).unwrap();

        let mut connection = unprocessed.process();
        connection.enqueue_ack();
        connection
    }

    fn deregister_unprocessed_connection(&mut self, token: &StreamToken) {
        if let Some(_) = self.unprocessed_connections.remove(&token) {
            let t = self.tokens.restore(*token);
            debug_assert!(t);
            let t = self.unprocessed_tokens.remove(&token);
            debug_assert!(t);
        } else {
            unreachable!()
        }
    }

    fn deregister_connection(&mut self, token: &StreamToken) {
        if let Some(_) = self.connections.remove(&token) {
            let t = self.tokens.restore(*token);
            debug_assert!(t);
        } else {
            unreachable!()
        }
    }

    fn create_connection(&mut self, stream: Stream, socket_address: &SocketAddr) -> IoHandlerResult<StreamToken> {
        let session = self.socket_to_session.remove(&socket_address).ok_or(Error::General("UnavailableSession"))?;
        let mut connection = Connection::new(stream, session.secret().clone(), session.nonce().clone());
        let nonce = session.nonce();
        connection.enqueue_sync(nonce.clone());

        Ok(self.tokens
            .gen()
            .map(|token| {
                self.register_connection(connection, &token);
                token
            })
            .ok_or(Error::General("TooManyConnections"))?)
    }

    pub fn accept(&mut self) -> IoHandlerResult<Option<(StreamToken, TimerToken, SocketAddr)>> {
        match self.listener.accept()? {
            Some((stream, socket_address)) => {
                let (stream_token, timer_token) = self.register_unprocessed_connection(stream)?;
                Ok(Some((stream_token, timer_token, socket_address)))
            }
            None => Ok(None),
        }
    }

    pub fn connect(&mut self, socket_address: &SocketAddr) -> IoHandlerResult<Option<StreamToken>> {
        Ok(match Stream::connect(socket_address)? {
            Some(stream) => Some(self.create_connection(stream, &socket_address)?),
            None => None,
        })
    }

    fn register_session(&mut self, socket_address: SocketAddr, session: Session) -> Result<()> {
        if self.socket_to_session.contains_key(&socket_address) {
            return Err(Error::General("SessionAlreadyRegistered"))
        }

        self.registered_sessions.insert(session.nonce().clone(), session.clone());
        self.socket_to_session.insert(socket_address, session);
        Ok(())
    }

    pub fn register_stream(
        &self,
        token: StreamToken,
        reg: Token,
        event_loop: &mut EventLoop<IoManager<Message>>,
    ) -> IoHandlerResult<()> {
        if let Some(connection) = self.connections.get(&token) {
            return Ok(connection.register(reg, event_loop)?)
        }

        let connection = self.unprocessed_connections.get(&token).ok_or(Error::InvalidStream(token))?;
        Ok(connection.register(reg, event_loop)?)
    }

    pub fn reregister_stream(
        &self,
        token: StreamToken,
        reg: Token,
        event_loop: &mut EventLoop<IoManager<Message>>,
    ) -> IoHandlerResult<()> {
        if let Some(connection) = self.connections.get(&token) {
            return Ok(connection.reregister(reg, event_loop)?)
        }

        let connection = self.unprocessed_connections.get(&token).ok_or(Error::InvalidStream(token))?;
        Ok(connection.reregister(reg, event_loop)?)
    }

    // return false if it's unprocessed connection
    fn deregister_stream(
        &self,
        token: StreamToken,
        event_loop: &mut EventLoop<IoManager<Message>>,
    ) -> IoHandlerResult<bool> {
        if let Some(connection) = self.connections.get(&token) {
            connection.deregister(event_loop)?;
            return Ok(true)
        }

        if let Some(connection) = self.unprocessed_connections.get(&token) {
            connection.deregister(event_loop)?;
            return Ok(false)
        }

        Err(From::from(Error::InvalidStream(token)))
    }

    // Return false if the received message is sync
    fn receive(&mut self, stream: &StreamToken, client: &Client) -> IoHandlerResult<bool> {
        if let Some(connection) = self.connections.get_mut(&stream) {
            return Ok(connection.receive(&ExtensionChannel::new(&client, *stream)))
        }

        {
            // connection borrows *self as mutable
            let connection = self.unprocessed_connections.get_mut(&stream).ok_or(Error::InvalidStream(stream.clone()))?;
            if let Some(_) = connection.receive(&self.registered_sessions)? {
                // Sync
            } else {
                return Ok(true)
            }
        }

        // receive Sync message
        let connection = self.process_connection(&stream);

        let session = connection.session().clone();
        let nonce = session.nonce().clone();

        // Session is not reusable
        let registered_session = self.registered_sessions.remove(&nonce);
        debug_assert_eq!(registered_session, Some(session));
        debug_assert!(registered_session.is_some());
        self.register_connection(connection, stream);
        client.on_node_added(&stream);
        Ok(false)
    }

    fn send(&mut self, stream: &StreamToken) -> IoHandlerResult<bool> {
        let connection = self.connections.get_mut(&stream).ok_or(Error::InvalidStream(stream.clone()))?;
        Ok(connection.send()?)
    }

    fn remove_waiting_sync_by_stream_token(&mut self, stream: &StreamToken) -> Option<UnprocessedConnection> {
        if let Some(timer) = self.waiting_sync_stream_to_timer.remove(&stream) {
            let t = self.waiting_sync_tokens.restore(timer);
            debug_assert!(t);

            let t = self.waiting_sync_timer_to_stream.remove(&stream);
            debug_assert!(t.is_some());

            let t = self.unprocessed_tokens.remove(&stream);
            debug_assert!(t);

            let t = self.unprocessed_connections.remove(&stream);
            debug_assert!(t.is_some());
            t
        } else {
            None
        }
    }

    fn remove_waiting_sync_by_timer_token(&mut self, timer: &TimerToken) {
        if let Some(stream) = self.waiting_sync_timer_to_stream.remove(&timer) {
            let t = self.waiting_sync_tokens.restore(*timer);
            debug_assert!(t);

            let t = self.waiting_sync_stream_to_timer.remove(&stream);
            debug_assert!(t.is_some());

            let t = self.unprocessed_tokens.remove(&stream);
            debug_assert!(t);

            let t = self.unprocessed_connections.remove(&stream);
            debug_assert!(t.is_some());
        }
    }
}

pub struct Handler {
    socket_address: SocketAddr,
    manager: Mutex<Manager>,
    client: Arc<Client>,

    node_token_to_socket: RwLock<HashMap<NodeToken, SocketAddr>>,
    socket_to_node_token: RwLock<HashMap<SocketAddr, NodeToken>>,
}

impl Handler {
    pub fn new(socket_address: SocketAddr, client: Arc<Client>) -> Self {
        let manager = Mutex::new(Manager::listen(&socket_address).expect("Cannot listen TCP port"));
        Self {
            socket_address,
            manager,
            client,

            node_token_to_socket: RwLock::new(HashMap::new()),
            socket_to_node_token: RwLock::new(HashMap::new()),
        }
    }
}

impl IoHandler<Message> for Handler {
    fn initialize(&self, io: &IoContext<Message>) -> IoHandlerResult<()> {
        io.register_stream(ACCEPT_TOKEN)?;
        Ok(())
    }

    fn timeout(&self, _io: &IoContext<Message>, token: TimerToken) -> IoHandlerResult<()> {
        match token {
            FIRST_WAIT_SYNC_TOKEN...LAST_WAIT_SYNC_TOKEN => {
                let mut manager = self.manager.lock();
                manager.remove_waiting_sync_by_timer_token(&token);
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    fn message(&self, io: &IoContext<Message>, message: &Message) -> IoHandlerResult<()> {
        match *message {
            Message::RegisterSession(ref socket_address, ref session) => {
                let mut manager = self.manager.lock();
                manager.register_session(socket_address.clone(), session.clone())?;
                Ok(())
            }
            Message::RequestConnection(ref socket_address, ref session) => {
                let mut manager = self.manager.lock();
                let _ = manager.register_session(socket_address.clone(), session.clone());

                info!("Connecting to {:?}", socket_address);
                let token = manager.connect(&socket_address)?.ok_or(Error::General("Cannot create connection"))?;
                io.register_stream(token)?;

                let mut node_token_to_socket = self.node_token_to_socket.write();
                let t = node_token_to_socket.insert(token, socket_address.clone());
                debug_assert!(t.is_none());

                let mut socket_to_node_token = self.socket_to_node_token.write();
                let t = socket_to_node_token.insert(socket_address.clone(), token);
                debug_assert!(t.is_none());
                Ok(())
            }
            Message::RequestNegotiation {
                node_id,
                ref extension_name,
                version,
            } => {
                let mut manager = self.manager.lock();
                let mut connection = manager.connections.get_mut(&node_id).ok_or(Error::InvalidNode(node_id))?;
                connection.enqueue_negotiation_request(extension_name.clone(), version);
                io.update_registration(node_id)?;
                Ok(())
            }
            Message::SendExtensionMessage {
                node_id,
                ref extension_name,
                ref need_encryption,
                ref data,
            } => {
                let mut manager = self.manager.lock();
                let mut connection = manager.connections.get_mut(&node_id).ok_or(Error::InvalidNode(node_id))?;
                connection.enqueue_extension_message(extension_name.clone(), *need_encryption, data.clone());
                io.update_registration(node_id)?;
                Ok(())
            }
        }
    }

    fn stream_hup(&self, io: &IoContext<Message>, stream: StreamToken) -> IoHandlerResult<()> {
        match stream {
            ACCEPT_TOKEN => unreachable!(),
            FIRST_CONNECTION_TOKEN...LAST_CONNECTION_TOKEN => {
                let mut node_token_to_socket = self.node_token_to_socket.write();
                let socket_address = node_token_to_socket.remove(&stream);
                debug_assert!(socket_address.is_some());
                if let Some(socket_address) = socket_address {
                    let mut socket_to_node_token = self.socket_to_node_token.write();
                    let t = socket_to_node_token.remove(&socket_address);
                    debug_assert!(t.is_some());
                }
                io.deregister_stream(stream)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    fn stream_readable(&self, io: &IoContext<Message>, stream: StreamToken) -> IoHandlerResult<()> {
        match stream {
            ACCEPT_TOKEN => loop {
                let mut manager = self.manager.lock();
                if let Some((token, timer_token, socket_address)) = manager.accept()? {
                    io.register_stream(token)?;
                    io.register_timer_once(timer_token, WAIT_SYNC_MS)?;
                    let mut node_token_to_socket = self.node_token_to_socket.write();
                    let t = node_token_to_socket.insert(token, socket_address.clone());
                    debug_assert!(t.is_none());

                    let mut socket_to_node_token = self.socket_to_node_token.write();
                    let t = socket_to_node_token.insert(socket_address, token);
                    debug_assert!(t.is_none());
                }
                break
            },
            FIRST_CONNECTION_TOKEN...LAST_CONNECTION_TOKEN => {
                let _f = finally(|| {
                    if let Err(err) = io.update_registration(stream) {
                        info!("Cannot update registration for connection {:?}", err);
                    }
                });
                loop {
                    let mut manager = self.manager.lock();
                    if !manager.receive(&stream, &self.client)? {
                        break
                    }
                }
            }
            _ => unimplemented!(),
        }
        Ok(())
    }

    fn stream_writable(&self, io: &IoContext<Message>, stream: StreamToken) -> IoHandlerResult<()> {
        match stream {
            ACCEPT_TOKEN => unreachable!(),
            FIRST_CONNECTION_TOKEN...LAST_CONNECTION_TOKEN => loop {
                let _f = finally(|| {
                    if let Err(err) = io.update_registration(stream) {
                        info!("Cannot update registration for connection {:?}", err);
                    }
                });
                let mut manager = self.manager.lock();
                if manager.unprocessed_tokens.contains(&stream) {
                    break
                }
                if !manager.send(&stream)? {
                    break
                }
            },
            _ => unimplemented!(),
        }
        Ok(())
    }

    fn register_stream(
        &self,
        stream: StreamToken,
        reg: Token,
        event_loop: &mut EventLoop<IoManager<Message>>,
    ) -> IoHandlerResult<()> {
        match stream {
            ACCEPT_TOKEN => {
                let manager = self.manager.lock();
                event_loop.register(&manager.listener, reg, Ready::readable(), PollOpt::edge())?;
                info!("TCP connection starts for {:?}", self.socket_address);
                Ok(())
            }
            FIRST_CONNECTION_TOKEN...LAST_CONNECTION_TOKEN => {
                let mut manager = self.manager.lock();
                manager.register_stream(stream, reg, event_loop)?;
                Ok(())
            }
            _ => {
                unreachable!();
            }
        }
    }

    fn update_stream(
        &self,
        stream: StreamToken,
        reg: Token,
        event_loop: &mut EventLoop<IoManager<Message>>,
    ) -> IoHandlerResult<()> {
        match stream {
            ACCEPT_TOKEN => {
                unreachable!();
            }
            FIRST_CONNECTION_TOKEN...LAST_CONNECTION_TOKEN => {
                let mut manager = self.manager.lock();
                manager.reregister_stream(stream, reg, event_loop)?;
                Ok(())
            }
            _ => {
                unreachable!();
            }
        }
    }

    fn deregister_stream(
        &self,
        stream: StreamToken,
        event_loop: &mut EventLoop<IoManager<Message>>,
    ) -> IoHandlerResult<()> {
        match stream {
            ACCEPT_TOKEN => unreachable!(),
            FIRST_CONNECTION_TOKEN...LAST_CONNECTION_TOKEN => {
                let mut manager = self.manager.lock();
                let is_processed = manager.deregister_stream(stream, event_loop)?;
                if is_processed {
                    manager.deregister_connection(&stream);
                } else {
                    manager.deregister_unprocessed_connection(&stream);
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}


pub trait AddressConverter: Send + Sync {
    fn node_token_to_address(&self, node: &NodeToken) -> Option<SocketAddr>;
    fn address_to_node_token(&self, address: &SocketAddr) -> Option<NodeToken>;
}

impl AddressConverter for Handler {
    fn node_token_to_address(&self, node_id: &NodeToken) -> Option<SocketAddr> {
        let node_id_to_socket = self.node_token_to_socket.read();
        node_id_to_socket.get(&node_id).map(|socket_address| socket_address.clone())
    }

    fn address_to_node_token(&self, socket_address: &SocketAddr) -> Option<NodeToken> {
        let socket_to_node_token = self.socket_to_node_token.read();
        socket_to_node_token.get(&socket_address).map(|id| id.clone())
    }
}