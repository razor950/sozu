use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use std::sync::{Arc,Mutex};
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::mem;
use std::net::Shutdown;
use mio::*;
use mio::net::*;
use mio_uds::UnixStream;
use mio::unix::UnixReady;
use std::os::unix::io::{AsRawFd};
use std::io::{self,Read,Write,ErrorKind,BufReader};
use std::collections::{HashMap,HashSet};
use std::error::Error;
use slab::{Slab,Entry,VacantEntry};
use std::net::{IpAddr,SocketAddr};
use std::str::{FromStr, from_utf8, from_utf8_unchecked};
use time::{precise_time_s, precise_time_ns};
use rand::random;
use rustls::{self, ServerConfig, ServerSession, NoClientAuth, ProtocolVersion,
  SupportedCipherSuite, ALL_CIPHERSUITES};
use mio_extras::timer::Timeout;

use sozu_command::buffer::Buffer;
use sozu_command::channel::Channel;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::proxy::{self,Application,CertFingerprint,CertificateAndKey,
  ProxyRequestData,HttpsFront,HttpsListener,ProxyRequest,ProxyResponse,
  ProxyResponseStatus,AddCertificate,RemoveCertificate,ReplaceCertificate,
  LoadBalancingParams, TlsVersion};
use sozu_command::certificate::split_certificate_chain;
use sozu_command::logging;

use parser::http11::{HttpState,RequestState,ResponseState,RRequestLine,parse_request_until_stop,hostname_and_port};
use network::buffer_queue::BufferQueue;
use network::pool::{Pool,Checkout};
use network::{AppId,Backend,SessionResult,ConnectionError,Protocol,Readiness,SessionMetrics,
  ProxySession,ProxyConfiguration,AcceptError,BackendConnectAction,BackendConnectionStatus};
use network::backends::BackendMap;
use network::server::{Server,ProxyChannel,ListenToken,ListenPortState,SessionToken,ListenSession,CONN_RETRIES};
use network::http::{self,DefaultAnswers, CustomAnswers};
use network::socket::{SocketHandler,SocketResult,server_bind,FrontRustls};
use network::trie::*;
use network::protocol::{ProtocolResult,TlsHandshake,Http,Pipe,StickySession};
use network::protocol::http::DefaultAnswerStatus;
use network::retry::RetryPolicy;
use util::UnwrapLog;

use super::resolver::{CertificateResolver,CertificateResolverWrapper};
use super::session::Session;

#[derive(Debug,Clone,PartialEq,Eq)]
pub struct TlsApp {
  pub app_id:           String,
  pub hostname:         String,
  pub path_begin:       String,
  pub cert_fingerprint: CertFingerprint,
}

pub type HostName  = String;
pub type PathBegin = String;

pub struct Listener {
  listener:   Option<TcpListener>,
  address:    SocketAddr,
  fronts:     TrieNode<Vec<TlsApp>>,
  pool:       Rc<RefCell<Pool<BufferQueue>>>,
  answers:    DefaultAnswers,
  config:     HttpsListener,
  ssl_config: Arc<ServerConfig>,
  resolver:   Arc<CertificateResolverWrapper>,
  pub token:  Token,
  active:     bool,
}

impl Listener {
  pub fn new(config: HttpsListener, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Listener {

    let default = DefaultAnswers {
      NotFound: Rc::new(Vec::from(config.answer_404.as_bytes())),
      ServiceUnavailable: Rc::new(Vec::from(config.answer_503.as_bytes())),
      BadRequest: Rc::new(Vec::from(
        &b"HTTP/1.1 400 Bad Request\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"[..]
      )),
    };

    let mut server_config = ServerConfig::new(NoClientAuth::new());
    server_config.versions = config.versions.iter().map(|version| {
      match version {
        TlsVersion::SSLv2   => ProtocolVersion::SSLv2,
        TlsVersion::SSLv3   => ProtocolVersion::SSLv3,
        TlsVersion::TLSv1_0 => ProtocolVersion::TLSv1_0,
        TlsVersion::TLSv1_1 => ProtocolVersion::TLSv1_1,
        TlsVersion::TLSv1_2 => ProtocolVersion::TLSv1_2,
        TlsVersion::TLSv1_3 => ProtocolVersion::TLSv1_3,
      }
    }).collect();

    let resolver = Arc::new(CertificateResolverWrapper::new());
    server_config.cert_resolver = resolver.clone();

    //FIXME: we should have another way than indexes in ALL_CIPHERSUITES,
    //but rustls does not export the static SupportedCipherSuite instances yet
    if !config.rustls_cipher_list.is_empty() {
      let mut ciphers = Vec::new();
      for cipher in config.rustls_cipher_list.iter() {
        match cipher.as_str() {
          "TLS13_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[0]),
          "TLS13_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[1]),
          "TLS13_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[2]),
          "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[3]),
          "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => ciphers.push(ALL_CIPHERSUITES[4]),
          "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[5]),
          "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[6]),
          "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => ciphers.push(ALL_CIPHERSUITES[7]),
          "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => ciphers.push(ALL_CIPHERSUITES[8]),
          s => error!("unknown cipher: {:?}", s),
        }
      }
      server_config.ciphersuites = ciphers;
    }

    Listener {
      address:    config.front.clone(),
      fronts:     TrieNode::root(),
      answers:    default,
      ssl_config: Arc::new(server_config),
      listener: None,
      pool,
      config,
      resolver,
      token,
      active: false,
    }
  }

  pub fn activate(&mut self, event_loop: &mut Poll, tcp_listener: Option<TcpListener>) -> Option<Token> {
    if self.active {
      return None;
    }

    let listener = tcp_listener.or_else(|| server_bind(&self.config.front).map_err(|e| {
      error!("could not create listener {:?}: {:?}", self.config.front, e);
    }).ok());


    if let Some(ref sock) = listener {
      event_loop.register(sock, self.token, Ready::readable(), PollOpt::edge());
    } else {
      return None;
    }

    self.listener = listener;
    self.active = true;
    Some(self.token)
  }

  pub fn add_https_front(&mut self, tls_front: HttpsFront, event_loop: &mut Poll) -> bool {
    if !(*self.resolver).add_front(&tls_front.fingerprint) {
      return false;
    }

    //FIXME: should clone he hostname then do a into() here
    let app = TlsApp {
      app_id:           tls_front.app_id.clone(),
      hostname:         tls_front.hostname.clone(),
      path_begin:       tls_front.path_begin.clone(),
      cert_fingerprint: tls_front.fingerprint.clone(),
    };

    if let Some((_,fronts)) = self.fronts.domain_lookup_mut(&tls_front.hostname.as_bytes()) {
        if ! fronts.contains(&app) {
          fronts.push(app.clone());
        }
    }
    if self.fronts.domain_lookup(&tls_front.hostname.as_bytes()).is_none() {
      self.fronts.domain_insert(tls_front.hostname.into_bytes(), vec![app]);
    }

    true
  }

  pub fn remove_https_front(&mut self, front: HttpsFront, event_loop: &mut Poll) {
    debug!("removing tls_front {:?}", front);

    let should_delete = {
      let fronts_opt = self.fronts.domain_lookup_mut(front.hostname.as_bytes());
      if let Some((_, fronts)) = fronts_opt {
        if let Some(pos) = fronts.iter()
          .position(|f| &f.app_id == &front.app_id && &f.cert_fingerprint == &front.fingerprint) {

          let front = fronts.remove(pos);
          (*self.resolver).remove_front(&front.cert_fingerprint) 
        }
      }

      fronts_opt.as_ref().map(|(_,fronts)| fronts.len() == 0).unwrap_or(false)
    };

    if should_delete {
      self.fronts.domain_remove(&front.hostname.into());
    }
  }

  pub fn add_certificate(&mut self, add_certificate: AddCertificate, event_loop: &mut Poll) -> bool {
    (*self.resolver).add_certificate(add_certificate).is_some()
  }

  // FIXME: return an error if the cert is still in use
  pub fn remove_certificate(&mut self, remove_certificate: RemoveCertificate, event_loop: &mut Poll) {
    debug!("removing certificate {:?}", remove_certificate);
    (*self.resolver).remove_certificate(remove_certificate)
  }

  pub fn replace_certificate(&mut self, replace_certificate: ReplaceCertificate, event_loop: &mut Poll) {
    debug!("replacing certificate {:?}", replace_certificate);
    let ReplaceCertificate { front, new_certificate, old_fingerprint, old_names, new_names } = replace_certificate;
    let remove = RemoveCertificate {
      front,
      fingerprint: old_fingerprint,
      names: old_names,
    };
    let add = AddCertificate {
      front,
      certificate: new_certificate,
      names: new_names,
    };

    //FIXME: handle results
    (*self.resolver).remove_certificate(remove);
    (*self.resolver).add_certificate(add);
  }

  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {

    if let Some(ref listener) = self.listener.as_ref() {
      listener.accept().map_err(|e| {
        match e.kind() {
          ErrorKind::WouldBlock => AcceptError::WouldBlock,
          other => {
            error!("accept() IO error: {:?}", e);
            AcceptError::IoError
          }
        }
      }).map(|(frontend_sock, _)| frontend_sock)
    } else {
      Err(AcceptError::IoError)
    }
  }

  // ToDo factor out with http.rs
  pub fn frontend_from_request(&self, host: &str, uri: &str) -> Option<&TlsApp> {
    let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(host.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
        return None;
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      unsafe { from_utf8_unchecked(hostname) }
    } else {
      error!("hostname parsing failed for: '{}'", host);
      return None;
    };

    if let Some((_,http_fronts)) = self.fronts.domain_lookup(host.as_bytes()) {
      let matching_fronts = http_fronts.iter().filter(|f| uri.starts_with(&f.path_begin)); // ToDo match on uri
      let mut front = None;

      for f in matching_fronts {
        if front.is_none() {
          front = Some(f);
        }

        if let Some(ff) = front {
          if f.path_begin.len() > ff.path_begin.len() {
            front = Some(f)
          }
        }
      }
      front
    } else {
      None
    }
  }

}

pub struct Proxy {
  listeners:      HashMap<Token, Listener>,
  applications:   HashMap<AppId, Application>,
  custom_answers: HashMap<AppId, CustomAnswers>,
  backends:       BackendMap,
  pool:           Rc<RefCell<Pool<BufferQueue>>>,
}

impl Proxy {
  pub fn new(pool: Rc<RefCell<Pool<BufferQueue>>>) -> Proxy {
    Proxy {
      listeners : HashMap::new(),
      applications: HashMap::new(),
      custom_answers: HashMap::new(),
      backends: BackendMap::new(),
      pool,
    }
  }

  pub fn add_listener(&mut self, config: HttpsListener, pool: Rc<RefCell<Pool<BufferQueue>>>, token: Token) -> Option<Token> {
    if self.listeners.contains_key(&token) {
      None
    } else {
      let listener = Listener::new(config, pool, token);
      self.listeners.insert(listener.token.clone(), listener);
      Some(token)
    }
  }

  pub fn activate_listener(&mut self, event_loop: &mut Poll, addr: &SocketAddr, tcp_listener: Option<TcpListener>) -> Option<Token> {
    for listener in self.listeners.values_mut() {
      if &listener.address == addr {
        return listener.activate(event_loop, tcp_listener);
      }
    }
    None
  }

  pub fn give_back_listeners(&mut self) -> Vec<(SocketAddr, TcpListener)> {
    self.listeners.values_mut().filter(|l| l.listener.is_some()).map(|l| {
      (l.address.clone(), l.listener.take().unwrap())
    }).collect()
  }

  pub fn add_application(&mut self, application: Application, event_loop: &mut Poll) {
    let app_id = &application.app_id.clone();
    let lb_alg = application.load_balancing_policy;

    self.applications.insert(application.app_id.clone(), application);
    self.backends.set_load_balancing_policy_for_app(app_id, lb_alg);
  }

  pub fn remove_application(&mut self, app_id: &str, event_loop: &mut Poll) {
    self.applications.remove(app_id);
    self.custom_answers.remove(app_id);
  }

  pub fn add_backend(&mut self, app_id: &str, backend: Backend,  event_loop: &mut Poll) {
    self.backends.add_backend(app_id, backend);
  }

  pub fn remove_backend(&mut self, app_id: &str, backend_address: &SocketAddr, event_loop: &mut Poll) {
    self.backends.remove_backend(app_id, backend_address);
  }

  pub fn backend_from_app_id(&mut self, session: &mut Session, app_id: &str, front_should_stick: bool) -> Result<TcpStream,ConnectionError> {
    session.http().map(|h| h.set_app_id(app_id.clone().to_string()));

    match self.backends.backend_from_app_id(&app_id) {
      Err(e) => {
        let answer = self.get_service_unavailable_answer(Some(app_id), &session.listen_token);
        session.set_answer(DefaultAnswerStatus::Answer503, answer);
        Err(e)
      },
      Ok((backend, conn))  => {
        if front_should_stick {
          let sticky_name = self.listeners[&session.listen_token].config.sticky_name.clone();
          session.http().map(|http| {
            http.sticky_session =
              Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or(backend.borrow().backend_id.clone())));
            http.sticky_name = sticky_name;
          });
        }
        session.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        session.metrics.backend_start();
        session.backend = Some(backend);

        Ok(conn)
      }
    }
  }

  pub fn backend_from_sticky_session(&mut self, session: &mut Session, app_id: &str, sticky_session: String) -> Result<TcpStream,ConnectionError> {
    session.http().map(|h| h.set_app_id(String::from(app_id)));

    match self.backends.backend_from_sticky_session(app_id, &sticky_session) {
      Err(e) => {
        debug!("Couldn't find a backend corresponding to sticky_session {} for app {}", sticky_session, app_id);
        let answer = self.get_service_unavailable_answer(Some(app_id), &session.listen_token);
        session.set_answer(DefaultAnswerStatus::Answer503, answer);
        Err(e)
      },
      Ok((backend, conn))  => {
        let sticky_name = self.listeners[&session.listen_token].config.sticky_name.clone();
        session.http().map(|http| {
          http.sticky_session = Some(StickySession::new(backend.borrow().sticky_id.clone().unwrap_or(sticky_session.clone())));
          http.sticky_name = sticky_name;
        });
        session.metrics.backend_id = Some(backend.borrow().backend_id.clone());
        session.metrics.backend_start();
        session.backend = Some(backend);

        Ok(conn)
      }
    }
  }

  fn app_id_from_request(&mut self, session: &mut Session) -> Result<String, ConnectionError> {
    let h = session.http().and_then(|h| h.state().get_host()).ok_or(ConnectionError::NoHostGiven)?;

    let host: &str = if let Ok((i, (hostname, port))) = hostname_and_port(h.as_bytes()) {
      if i != &b""[..] {
        error!("invalid remaining chars after hostname");
        let answer = self.listeners[&session.listen_token].answers.BadRequest.clone();
        session.set_answer(DefaultAnswerStatus::Answer400, answer);
        return Err(ConnectionError::InvalidHost);
      }

      // it is alright to call from_utf8_unchecked,
      // we already verified that there are only ascii
      // chars in there
      let hostname_str =  unsafe { from_utf8_unchecked(hostname) };

      //FIXME: what if we don't use SNI?
      let servername: Option<String> = session.http()
        .and_then(|h| h.frontend.session.get_sni_hostname()).map(|s| s.to_string());
      if servername.as_ref().map(|s| s.as_str()) != Some(hostname_str) {
        error!("TLS SNI hostname '{:?}' and Host header '{}' don't match", servername, hostname_str);
        let answer = self.listeners[&session.listen_token].answers.NotFound.clone();
        unwrap_msg!(session.http()).set_answer(DefaultAnswerStatus::Answer404, answer);
        return Err(ConnectionError::HostNotFound);
      }

      //FIXME: we should check that the port is right too

      if port == Some(&b"443"[..]) {
        hostname_str
      } else {
        &h
      }
    } else {
      error!("hostname parsing failed");
      let answer = self.listeners[&session.listen_token].answers.BadRequest.clone();
      session.set_answer(DefaultAnswerStatus::Answer400, answer);
      return Err(ConnectionError::InvalidHost);
    };

    let rl:RRequestLine = session.http()
      .and_then(|h| h.state().get_request_line()).ok_or(ConnectionError::NoRequestLineGiven)?;
    match self.listeners.get(&session.listen_token).as_ref()
      .and_then(|l| l.frontend_from_request(&host, &rl.uri))
      .map(|ref front| front.app_id.clone()) {
      Some(app_id) => Ok(app_id),
      None => {
        let answer = self.listeners[&session.listen_token].answers.NotFound.clone();
        session.set_answer(DefaultAnswerStatus::Answer404, answer);
        Err(ConnectionError::HostNotFound)
      }
    }
  }

  fn check_circuit_breaker(&mut self, session: &mut Session) -> Result<(), ConnectionError> {
    if session.connection_attempt == CONN_RETRIES {
      error!("{} max connection attempt reached", session.log_context());
      let answer = self.get_service_unavailable_answer(session.app_id.as_ref().map(|app_id| app_id.as_str()), &session.listen_token);
      session.set_answer(DefaultAnswerStatus::Answer503, answer);
      Err(ConnectionError::NoBackendAvailable)
    } else {
      Ok(())
    }
  }

  fn get_service_unavailable_answer(&self, app_id: Option<&str>, listen_token: &Token) -> Rc<Vec<u8>> {
    if let Some(answer_service_unavailable) = app_id.and_then(|app_id| self.custom_answers.get(app_id).and_then(|c| c.ServiceUnavailable.as_ref())) {
      answer_service_unavailable.clone()
    } else {
      self.listeners[&listen_token].answers.ServiceUnavailable.clone()
    }
  }
}

impl ProxyConfiguration<Session> for Proxy {
  fn accept(&mut self, token: ListenToken) -> Result<TcpStream, AcceptError> {
    self.listeners.get_mut(&Token(token.0)).unwrap().accept(token)
  }

  fn create_session(&mut self, frontend_sock: TcpStream, token: ListenToken, poll: &mut Poll, session_token: Token, timeout: Timeout)
    -> Result<(Rc<RefCell<Session>>,bool), AcceptError> {
      if let Some(ref listener) = self.listeners.get(&Token(token.0)) {
        frontend_sock.set_nodelay(true);

        poll.register(
          &frontend_sock,
          session_token,
          Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
          PollOpt::edge()
        );

        let session = ServerSession::new(&listener.ssl_config);
        let c = Session::new(session, frontend_sock, session_token, Rc::downgrade(&self.pool), listener.config.public_address,
          listener.config.expect_proxy, listener.config.sticky_name.clone(), timeout, Token(token.0));

        Ok((Rc::new(RefCell::new(c)), false))
      } else {
        //FIXME
        Err(AcceptError::IoError)
      }
    }

  fn connect_to_backend(&mut self, poll: &mut Poll,  session: &mut Session, back_token: Token) -> Result<BackendConnectAction,ConnectionError> {
    let old_app_id = session.http().and_then(|ref http| http.app_id.clone());
    let old_back_token = session.back_token();

    self.check_circuit_breaker(session)?;

    let app_id = self.app_id_from_request(session)?;

    if (session.http().and_then(|h| h.app_id.as_ref()) == Some(&app_id)) && session.back_connected == BackendConnectionStatus::Connected {
      if session.backend.as_ref().map(|backend| {
         let ref backend = *backend.borrow();
         self.backends.has_backend(&app_id, backend)
      }).unwrap_or(false) {
        //matched on keepalive
        session.metrics.backend_id = session.backend.as_ref().map(|i| i.borrow().backend_id.clone());
        session.metrics.backend_start();
        return Ok(BackendConnectAction::Reuse);
      } else {
        if let Some(token) = session.back_token() {
          session.close_backend(token, poll);

          //reset the back token here so we can remove it
          //from the slab after backend_from* fails
          session.set_back_token(token);
        }
      }
    }

    if old_app_id.is_some() && old_app_id.as_ref() != Some(&app_id) {
      if let Some(token) = session.back_token() {
        session.close_backend(token, poll);

        //reset the back token here so we can remove it
        //from the slab after backend_from* fails
        session.set_back_token(token);
      }
    }

    session.app_id = Some(app_id.clone());

    let sticky_session = session.http().and_then(|http| http.state.as_ref()).unwrap().get_request_sticky_session();
    let front_should_stick = self.applications.get(&app_id).map(|ref app| app.sticky_session).unwrap_or(false);
    let socket = match (front_should_stick, sticky_session) {
      (true, Some(sticky_session)) => self.backend_from_sticky_session(session, &app_id, sticky_session)?,
      _ => self.backend_from_app_id(session, &app_id, front_should_stick)?,
    };

    session.http().map(|http| {
      http.app_id = Some(app_id.clone());
      http.reset_log_context();
    });

    // we still want to use the new socket
    socket.set_nodelay(true);
    session.back_readiness().map(|r| {
      r.interest  = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
    });

    session.back_connected = BackendConnectionStatus::Connecting;
    if let Some(back_token) = old_back_token {
      session.set_back_token(back_token);
      poll.register(
        &socket,
        back_token,
        Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
        PollOpt::edge()
      );

      session.set_back_socket(socket);
      Ok(BackendConnectAction::Replace)
    } else {
      poll.register(
        &socket,
        back_token,
        Ready::readable() | Ready::writable() | Ready::from(UnixReady::hup() | UnixReady::error()),
        PollOpt::edge()
      );

      session.set_back_socket(socket);
      session.set_back_token(back_token);
      Ok(BackendConnectAction::New)
    }
  }

  fn notify(&mut self, event_loop: &mut Poll, message: ProxyRequest) -> ProxyResponse {
    //info!("{} notified", message);
    match message.order {
      ProxyRequestData::AddApplication(application) => {
        debug!("{} add application {:?}", message.id, application);
        self.add_application(application, event_loop);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::RemoveApplication(application) => {
        debug!("{} remove application {:?}", message.id, application);
        self.remove_application(&application, event_loop);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::AddHttpsFront(front) => {
        //info!("HTTPS\t{} add front {:?}", id, front);
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.add_https_front(front, event_loop);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!("unknown listener: {:?}", front.address)
        }
      },
      ProxyRequestData::RemoveHttpsFront(front) => {
        //info!("HTTPS\t{} remove front {:?}", id, front);
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == front.address) {
          listener.remove_https_front(front, event_loop);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!("unknown listener: {:?}", front.address)
        }
      },
      ProxyRequestData::AddCertificate(add_certificate) => {
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == add_certificate.front) {
          listener.add_certificate(add_certificate, event_loop);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!()
        }
      },
      ProxyRequestData::RemoveCertificate(remove_certificate) => {
        //FIXME: should return an error if certificate still has fronts referencing it
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == remove_certificate.front) {
          listener.remove_certificate(remove_certificate, event_loop);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!()
        }
      },
      ProxyRequestData::ReplaceCertificate(replace_certificate) => {
        //FIXME: should return an error if certificate still has fronts referencing it
        if let Some(mut listener) = self.listeners.values_mut().find(|l| l.address == replace_certificate.front) {
          listener.replace_certificate(replace_certificate, event_loop);
          ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
        } else {
          panic!()
        }
      },
      ProxyRequestData::AddBackend(backend) => {
        debug!("{} add backend {:?}", message.id, backend);
        let new_backend = Backend::new(&backend.backend_id, backend.address.clone(), backend.sticky_id.clone(), backend.load_balancing_parameters, backend.backup);
        self.add_backend(&backend.app_id, new_backend, event_loop);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::RemoveBackend(backend) => {
        debug!("{} remove backend {:?}", message.id, backend);
        self.remove_backend(&backend.app_id, &backend.address, event_loop);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::RemoveListener(remove) => {
        info!("removing https listener at address: {:?}", remove.front);
        fixme!();
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::SoftStop => {
        info!("{} processing soft shutdown", message.id);
        self.listeners.iter_mut().map(|(token, l)| {
          l.listener.take().map(|sock| {
            event_loop.deregister(&sock);
          })
        });
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Processing, data: None }
      },
      ProxyRequestData::HardStop => {
        info!("{} hard shutdown", message.id);
        self.listeners.drain().map(|(token, mut l)| {
          l.listener.take().map(|sock| {
            event_loop.deregister(&sock);
          })
        });
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Processing, data: None }
      },
      ProxyRequestData::Status => {
        debug!("{} status", message.id);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      ProxyRequestData::Logging(logging_filter) => {
        debug!("{} changing logging filter to {}", message.id, logging_filter);
        logging::LOGGER.with(|l| {
          let directives = logging::parse_logging_spec(&logging_filter);
          l.borrow_mut().set_directives(directives);
        });
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Ok, data: None }
      },
      command => {
        error!("{} unsupported message, ignoring {:?}", message.id, command);
        ProxyResponse{ id: message.id, status: ProxyResponseStatus::Error(String::from("unsupported message")), data: None }
      }
    }
  }

  fn listen_port_state(&self, port: &u16) -> ListenPortState {
    fixme!();
    ListenPortState::Available
    //if port == &self.address.port() { ListenPortState::InUse } else { ListenPortState::Available }
  }
}

use network::server::HttpsProvider;
pub fn start(config: HttpsListener, channel: ProxyChannel, max_buffers: usize, buffer_size: usize) {
  use network::server::{self,ProxySessionCast};

  let mut event_loop  = Poll::new().expect("could not create event loop");
  let max_listeners   = 1;

  let pool = Rc::new(RefCell::new(
    Pool::with_capacity(2*max_buffers, 0, || BufferQueue::with_capacity(buffer_size))
  ));

  let mut sessions: Slab<Rc<RefCell<ProxySessionCast>>,SessionToken> = Slab::with_capacity(max_buffers);
  {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    info!("taking token {:?} for channel", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    info!("taking token {:?} for timer", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }
  {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    info!("taking token {:?} for metrics", entry.index());
    entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
  }

  let token = {
    let entry = sessions.vacant_entry().expect("session list should have enough room at startup");
    let e = entry.insert(Rc::new(RefCell::new(ListenSession { protocol: Protocol::HTTPListen })));
    Token(e.index().0)
  };

  let front = config.front.clone();
  let mut configuration = Proxy::new(pool.clone());
  if configuration.add_listener(config, pool.clone(), token).is_some() {
    if configuration.activate_listener(&mut event_loop, &front, None).is_some() {
      let (scm_server, scm_client) = UnixStream::pair().unwrap();
      let mut server_config: server::ServerConfig = Default::default();
      server_config.max_connections = max_buffers;
      let mut server  = Server::new(event_loop, channel, ScmSocket::new(scm_server.as_raw_fd()),
        sessions, pool, None, Some(HttpsProvider::Rustls(configuration)), None, server_config, None);

      info!("starting event loop");
      server.run();
      info!("ending event loop");
    }
  }
}

