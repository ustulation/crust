// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

pub use self::errors::ServiceDiscoveryError;

mod errors;

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::u16;

use common::{Context, Core, State};
use maidsafe_utilities::serialisation::{deserialise, serialise};
use nat::MappedAddr;
use rand;
use socket_addr;

use mio::udp::UdpSocket;
use mio::{EventLoop, EventSet, PollOpt, Token};

#[derive(RustcEncodable, RustcDecodable)]
enum DiscoveryMsg {
    Request {
        guid: u64,
    },
    Response(Vec<socket_addr::SocketAddr>),
}

pub struct ServiceDiscovery {
    token: Token,
    socket: UdpSocket,
    remote_addr: SocketAddr,
    listen: bool,
    read_buf: [u8; 1024],
    our_listeners: Arc<Mutex<Vec<MappedAddr>>>,
    seek_peers_req: Vec<u8>,
    reply_to: VecDeque<SocketAddr>,
    observers: Vec<Sender<Vec<socket_addr::SocketAddr>>>,
    guid: u64,
}

impl ServiceDiscovery {
    pub fn start(core: &mut Core,
                 event_loop: &mut EventLoop<Core>,
                 our_listeners: Arc<Mutex<Vec<MappedAddr>>>,
                 context: Context,
                 port: u16)
                 -> Result<(), ServiceDiscoveryError> {
        let token = core.get_new_token();

        let udp_socket = try!(get_socket(port));
        try!(udp_socket.set_broadcast(true));

        let guid = rand::random();
        let remote_addr = try!(SocketAddr::from_str(&format!("255.255.255.255:{}", port)));

        let service_discovery = ServiceDiscovery {
            token: token,
            socket: udp_socket,
            remote_addr: remote_addr,
            listen: false,
            read_buf: [0; 1024],
            our_listeners: our_listeners,
            seek_peers_req: try!(serialise(&DiscoveryMsg::Request { guid: guid })),
            reply_to: VecDeque::new(),
            observers: Vec::new(),
            guid: guid,
        };

        try!(event_loop.register(&service_discovery.socket,
                                 service_discovery.token,
                                 EventSet::error() | EventSet::hup() | EventSet::readable(),
                                 PollOpt::edge()));

        let _ = core.insert_context(token, context);
        let _ = core.insert_state(context, Rc::new(RefCell::new(service_discovery)));

        Ok(())
    }

    /// Enable/disable listening and responding to peers searching for us. This will allow others
    /// finding us by interrogating the network.
    pub fn set_listen(&mut self, listen: bool) {
        self.listen = listen;
    }

    /// Interrogate the network to find peers.
    pub fn seek_peers(&mut self) -> Result<(), ServiceDiscoveryError> {
        let _ = try!(self.socket.send_to(&self.seek_peers_req, &self.remote_addr));
        Ok(())
    }

    /// Register service discovery observer
    pub fn register_observer(&mut self, obs: Sender<Vec<socket_addr::SocketAddr>>) {
        self.observers.push(obs);
    }

    fn read(&mut self, core: &mut Core, event_loop: &mut EventLoop<Core>) {
        let (bytes_rxd, peer_addr) = match self.socket.recv_from(&mut self.read_buf) {
            Ok(Some((bytes_rxd, peer_addr))) => (bytes_rxd, peer_addr),
            Ok(None) => return,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => return,
            Err(e) => {
                warn!("ServiceDiscovery error in read: {:?}", e);
                self.terminate(core, event_loop);
                return;
            }
        };

        let msg: DiscoveryMsg = match deserialise(&self.read_buf[..bytes_rxd]) {
            Ok(msg) => msg,
            Err(e) => {
                warn!("Bogus message serialisation error: {:?}", e);
                return;
            }
        };

        match msg {
            DiscoveryMsg::Request { guid } => {
                if self.listen && self.guid != guid {
                    self.reply_to.push_back(peer_addr);
                    self.write(core, event_loop)
                }
            }
            DiscoveryMsg::Response(peer_listeners) => {
                self.observers.retain(|obs| obs.send(peer_listeners.clone()).is_ok());
            }
        }
    }

    fn write(&mut self, core: &mut Core, event_loop: &mut EventLoop<Core>) {
        if let Err(e) = self.write_impl(event_loop) {
            warn!("Error in ServiceDiscovery write: {:?}", e);
            self.terminate(core, event_loop);
        }
    }

    fn write_impl(&mut self,
                  event_loop: &mut EventLoop<Core>)
                  -> Result<(), ServiceDiscoveryError> {
        let our_current_listeners = self.our_listeners
            .lock()
            .unwrap()
            .iter()
            .filter(|elt| !elt.nat_restricted)
            .map(|elt| elt.addr)
            .collect();
        let resp = DiscoveryMsg::Response(our_current_listeners);

        let serialised_resp = try!(serialise(&resp));

        if let Some(peer_addr) = self.reply_to.pop_front() {
            match self.socket.send_to(&serialised_resp[..], &peer_addr) {
                // UDP is all or none so if anything is written we consider it written
                Ok(Some(_)) => (),
                Ok(None) => self.reply_to.push_front(peer_addr),
                Err(ref e) if e.kind() == ErrorKind::Interrupted ||
                              e.kind() == ErrorKind::WouldBlock => {
                    self.reply_to
                        .push_front(peer_addr)
                }
                Err(e) => return Err(From::from(e)),
            }
        }

        let event_set = if self.reply_to.is_empty() {
            EventSet::error() | EventSet::hup() | EventSet::readable()
        } else {
            EventSet::error() | EventSet::hup() | EventSet::readable() | EventSet::writable()
        };

        Ok(try!(event_loop.reregister(&self.socket, self.token, event_set, PollOpt::edge())))
    }
}

impl State for ServiceDiscovery {
    fn ready(&mut self,
             core: &mut Core,
             event_loop: &mut EventLoop<Core>,
             _: Token,
             event_set: EventSet) {
        if event_set.is_error() || event_set.is_hup() {
            self.terminate(core, event_loop);
        } else {
            if event_set.is_readable() {
                self.read(core, event_loop);
            }
            if event_set.is_writable() {
                self.write(core, event_loop);
            }
        }
    }

    fn terminate(&mut self, core: &mut Core, event_loop: &mut EventLoop<Core>) {
        if let Err(e) = event_loop.deregister(&self.socket) {
            warn!("Error deregistering ServiceDiscovery: {:?}", e);
        }
        if let Some(context) = core.remove_context(self.token) {
            let _ = core.remove_state(context);
        }
    }

    fn as_any(&mut self) -> &mut Any {
        self
    }
}

fn get_socket(mut port: u16) -> Result<UdpSocket, ServiceDiscoveryError> {
    let mut res;
    loop {
        let bind_addr = try!(SocketAddr::from_str(&format!("0.0.0.0:{}", port)));
        let udp_socket = try!(UdpSocket::v4());
        match udp_socket.bind(&bind_addr) {
            Ok(()) => {
                res = Ok(udp_socket);
                break;
            }
            Err(e) => {
                res = Err(From::from(e));
            }
        }
        if port == u16::MAX {
            break;
        }
        port += 1;
    }

    res
}

#[cfg(test)]
mod test {
    use super::*;

    use std::net;
    use std::str::FromStr;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use common::{Context, Core, CoreMessage};
    use maidsafe_utilities::thread::RaiiThreadJoiner;
    use mio::EventLoop;
    use nat::MappedAddr;

    #[test]
    fn service_discovery() {
        // EventLoop-0
        let mut el0 = EventLoop::new().expect("Could not spawn el0");
        let tx0 = el0.channel();
        let _raii_joiner_0 = RaiiThreadJoiner::new(thread!("EL0", move || {
            el0.run(&mut Core::new()).expect("Could not run el0");
        }));

        let addr = net::SocketAddr::from_str("138.139.140.150:54321").unwrap();
        let mapped_addr = MappedAddr::new(addr, false);
        let listeners_0 = Arc::new(Mutex::new(vec![mapped_addr]));
        let listeners_0_clone = listeners_0.clone();

        // ServiceDiscovery-0
        {
            let context_0 = Context(0);
            tx0.send(CoreMessage::new(move |core, el| {
                    ServiceDiscovery::start(core, el, listeners_0_clone, context_0, 65530)
                        .expect("Could not spawn ServiceDiscovery_0");
                }))
                .expect("Could not send to tx0");

            // Start listening for peers
            tx0.send(CoreMessage::new(move |core, _| {
                    let state = core.get_state(context_0).unwrap();
                    let mut inner = state.borrow_mut();
                    inner.as_any().downcast_mut::<ServiceDiscovery>().unwrap().set_listen(true);
                }))
                .unwrap();
        }

        thread::sleep(Duration::from_millis(100));

        // EventLoop-1
        let mut el1 = EventLoop::new().expect("Could not spawn el1");
        let tx1 = el1.channel();
        let _raii_joiner_1 = RaiiThreadJoiner::new(thread!("EL1", move || {
            el1.run(&mut Core::new()).expect("Could not run el1");
        }));

        let (tx, rx) = mpsc::channel();

        // ServiceDiscovery-1
        {
            let listeners_1 = Arc::new(Mutex::new(vec![]));
            let context_1 = Context(0);
            tx1.send(CoreMessage::new(move |core, el| {
                    ServiceDiscovery::start(core, el, listeners_1, context_1, 65530)
                        .expect("Could not spawn ServiceDiscovery_1");
                }))
                .expect("Could not send to tx1");

            // Register observer
            tx1.send(CoreMessage::new(move |core, _| {
                    let state = core.get_state(context_1).unwrap();
                    let mut inner = state.borrow_mut();
                    inner.as_any()
                        .downcast_mut::<ServiceDiscovery>()
                        .unwrap()
                        .register_observer(tx);
                }))
                .unwrap();

            // Seek peers
            tx1.send(CoreMessage::new(move |core, _| {
                    let state = core.get_state(context_1).unwrap();
                    let mut inner = state.borrow_mut();
                    inner.as_any()
                        .downcast_mut::<ServiceDiscovery>()
                        .unwrap()
                        .seek_peers()
                        .unwrap();
                }))
                .expect("Could not send to tx1");
        }

        let peer_listeners = rx.recv().unwrap();
        assert_eq!(peer_listeners,
                   listeners_0.lock().unwrap().iter().map(|elt| elt.addr).collect::<Vec<_>>());

        tx0.send(CoreMessage::new(move |_, el| el.shutdown())).expect("Could not shutdown el0");
        tx1.send(CoreMessage::new(move |_, el| el.shutdown())).expect("Could not shutdown el1");
    }
}