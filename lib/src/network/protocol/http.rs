use std::cmp::min;
use std::io::Write;
use std::rc::{Rc,Weak};
use std::cell::RefCell;
use std::net::{SocketAddr,IpAddr};
use mio::*;
use mio::unix::UnixReady;
use mio::tcp::TcpStream;
use time::{Duration, precise_time_s, precise_time_ns};
use uuid::Uuid;
use parser::http11::{HttpState,parse_request_until_stop, parse_response_until_stop,
  BufferMove, RequestState, ResponseState, Chunk, Continue, RRequestLine, RStatusLine};
use network::{SessionResult,Protocol,Readiness,SessionMetrics, LogDuration};
use network::buffer_queue::BufferQueue;
use network::socket::{SocketHandler,SocketResult};
use network::protocol::ProtocolResult;
use network::pool::{Pool,Checkout};
use pool_crate::Reset;
use util::UnwrapLog;

#[derive(Clone)]
pub struct StickySession {
  pub sticky_id: String
}

impl StickySession {
  pub fn new(backend_id: String) -> StickySession {
    StickySession {
      sticky_id: backend_id
    }
  }
}

type BackendToken = Token;

#[derive(Debug,Clone,PartialEq)]
pub enum SessionStatus {
  Normal,
  /// status, HTTP answer, index in HTTP answer
  DefaultAnswer(DefaultAnswerStatus, Rc<Vec<u8>>, usize),
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub enum DefaultAnswerStatus {
  Answer301,
  Answer400,
  Answer404,
  Answer503,
  Answer413,
}

pub struct Http<Front:SocketHandler> {
  pub frontend:       Front,
  pub backend:        Option<TcpStream>,
  frontend_token:     Token,
  backend_token:      Option<Token>,
  pub status:         SessionStatus,
  pub state:          Option<HttpState>,
  pub front_buf:      Option<Checkout<BufferQueue>>,
  pub back_buf:       Option<Checkout<BufferQueue>>,
  pub app_id:         Option<String>,
  pub request_id:     String,
  pub front_readiness:Readiness,
  pub back_readiness: Readiness,
  pub log_ctx:        String,
  pub public_address: Option<IpAddr>,
  pub session_address: Option<SocketAddr>,
  pub sticky_name:    String,
  pub sticky_session: Option<StickySession>,
  pub protocol:       Protocol,
  pool:               Weak<RefCell<Pool<BufferQueue>>>,
}

impl<Front:SocketHandler> Http<Front> {
  pub fn new(sock: Front, token: Token, pool: Weak<RefCell<Pool<BufferQueue>>>,
    public_address: Option<IpAddr>, session_address: Option<SocketAddr>, sticky_name: String,
    protocol: Protocol) -> Option<Http<Front>> {

    let request_id = Uuid::new_v4().hyphenated().to_string();
    let log_ctx    = format!("{} unknown\t", &request_id);
    let mut session = Http {
      frontend:           sock,
      backend:            None,
      frontend_token:     token,
      backend_token:      None,
      status:             SessionStatus::Normal,
      state:              Some(HttpState::new()),
      front_buf:          None,
      back_buf:           None,
      app_id:             None,
      request_id:         request_id,
      front_readiness:    Readiness::new(),
      back_readiness:     Readiness::new(),
      log_ctx:            log_ctx,
      public_address:     public_address,
      session_address:     session_address,
      sticky_name:        sticky_name,
      sticky_session:     None,
      protocol:           protocol,
      pool,
    };
    let req_header = session.added_request_header(public_address, session_address);
    let res_header = session.added_response_header();
    session.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    session.state.as_mut().map(|ref mut state| state.added_res_header = res_header);

    Some(session)
  }

  pub fn reset(&mut self) {
    let request_id = Uuid::new_v4().hyphenated().to_string();
    //info!("{} RESET TO {}", self.log_ctx, request_id);
    gauge_add!("http.active_requests", -1);
    self.state.as_mut().map(|state| state.reset());
    let req_header = self.added_request_header(self.public_address, self.session_address);
    let res_header = self.added_response_header();
    self.state.as_mut().map(|ref mut state| state.added_req_header = req_header);
    self.state.as_mut().map(|ref mut state| state.added_res_header = res_header);

    // if HTTP requests are pipelined, we might still have some data in the front buffer
    if self.front_buf.as_ref().map(|buf| !buf.empty()).unwrap_or(false) {
      self.front_readiness.event.insert(Ready::readable());
    } else {
      self.front_buf = None;
    }

    self.back_buf = None;
    self.request_id = request_id;
    self.reset_log_context();
  }

  pub fn reset_log_context(&mut self) {
    self.log_ctx = format!("{} {}\t",
      self.request_id, self.app_id.as_ref().unwrap_or(&String::from("unknown")));
  }

  fn tokens(&self) -> Option<(Token,Token)> {
    if let Some(back) = self.backend_token {
      return Some((self.frontend_token, back))
    }
    None
  }

  pub fn state(&mut self) -> &mut HttpState {
    unwrap_msg!(self.state.as_mut())
  }

  pub fn set_state(&mut self, state: HttpState) {
    self.state = Some(state);
  }

  pub fn set_answer(&mut self, answer: DefaultAnswerStatus, buf: Rc<Vec<u8>>)  {
    self.front_buf = None;
    self.back_buf = None;

    if let SessionStatus::DefaultAnswer(status, _, _) = self.status {
      error!("already set the default answer to {:?}, trying to set to {:?}", status, answer);
    } else {
      match answer {
        DefaultAnswerStatus::Answer301 => incr!("http.301.redirection"),
        DefaultAnswerStatus::Answer400 => incr!("http.400.errors"),
        DefaultAnswerStatus::Answer404 => incr!("http.404.errors"),
        DefaultAnswerStatus::Answer413 => incr!("http.413.errors"),
        DefaultAnswerStatus::Answer503 => incr!("http.503.errors"),
      };
    }

    self.status = SessionStatus::DefaultAnswer(answer, buf, 0);
    self.front_readiness.interest = UnixReady::from(Ready::writable()) | UnixReady::hup() | UnixReady::error();
    self.back_readiness.interest  = UnixReady::hup() | UnixReady::error();

  }

  pub fn added_request_header(&self, public_address: Option<IpAddr>, session_address: Option<SocketAddr>) -> String {
    let peer = session_address.or(self.front_socket().peer_addr().ok()).map(|addr| (addr.ip(), addr.port()));
    let front = public_address.or(self.front_socket().local_addr().map(|addr| addr.ip()).ok());
    let session_port = self.front_socket().local_addr().map(|addr| addr.port()).ok();
    if let (Some((peer_ip, peer_port)), Some(front), Some(session_port)) = (peer, front, session_port) {
      let proto = match self.protocol() {
        Protocol::HTTP  => "http",
        Protocol::HTTPS => "https",
        _               => unreachable!()
      };

      //FIXME: in the "for", we don't put the other values we could get from a preexisting forward header
      match (peer_ip, peer_port, front) {
        (IpAddr::V4(p), peer_port, IpAddr::V4(f)) => {
          format!("Forwarded: proto={};for={}:{};by={}\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, session_port, self.request_id)
        },
        (IpAddr::V4(p), peer_port, IpAddr::V6(f)) => {
          format!("Forwarded: proto={};for={}:{};by=\"{}\"\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, session_port, self.request_id)
        },
        (IpAddr::V6(p), peer_port, IpAddr::V4(f)) => {
          format!("Forwarded: proto={};for=\"{}:{}\";by={}\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, session_port, self.request_id)
        },
        (IpAddr::V6(p), peer_port, IpAddr::V6(f)) => {
          format!("Forwarded: proto={};for=\"{}:{}\";by=\"{}\"\r\nX-Forwarded-Proto: {}\r\nX-Forwarded-For: {}\r\n\
                  X-Forwarded-Port: {}\r\nSozu-Id: {}\r\n",
            proto, peer_ip, peer_port, front, proto, peer_ip, session_port, self.request_id)
        },
      }
    } else {
      format!("Sozu-Id: {}\r\n", self.request_id)
    }
  }

  pub fn added_response_header(&self) -> String {
    format!("Sozu-Id: {}\r\n", self.request_id)
  }

  pub fn front_socket(&self) -> &TcpStream {
    self.frontend.socket_ref()
  }

  pub fn back_socket(&self)  -> Option<&TcpStream> {
    self.backend.as_ref()
  }

  pub fn back_token(&self)   -> Option<Token> {
    self.backend_token
  }

  pub fn close(&mut self) {
  }

  pub fn log_context(&self) -> String {
    if let Some(ref app_id) = self.app_id {
      format!("{}\t{}\t", self.request_id, app_id)
    } else {
      format!("{}\tunknown\t", self.request_id)
    }
  }

  pub fn set_back_socket(&mut self, socket: TcpStream) {
    self.backend         = Some(socket);
  }

  pub fn set_app_id(&mut self, app_id: String) {
    self.log_ctx = format!("{} {}\t",
      self.request_id, &app_id);

    self.app_id  = Some(app_id);
  }

  pub fn set_back_token(&mut self, token: Token) {
    self.backend_token = Some(token);
  }

  pub fn clear_back_token(&mut self) {
    self.backend_token = None;
  }

  pub fn front_readiness(&mut self) -> &mut Readiness {
    &mut self.front_readiness
  }

  pub fn back_readiness(&mut self) -> &mut Readiness {
    &mut self.back_readiness
  }

  fn protocol(&self) -> Protocol {
    self.protocol
  }

  pub fn remove_backend(&mut self) -> (Option<String>, Option<SocketAddr>) {
    debug!("{}\tPROXY [{} -> {}] CLOSED BACKEND", self.log_ctx, self.frontend_token.0,
      self.backend_token.map(|t| format!("{}", t.0)).unwrap_or("-".to_string()));
    let addr:Option<SocketAddr> = self.backend.as_ref().and_then(|sock| sock.peer_addr().ok());
    self.backend       = None;
    self.backend_token = None;
    (self.app_id.clone(), addr)
  }

  pub fn front_hup(&mut self) -> SessionResult {
    SessionResult::CloseSession
  }

  pub fn back_hup(&mut self) -> SessionResult {
    if let Some(ref mut buf) = self.back_buf {
      //FIXME: closing the session might not be a good idea if we do keep alive on the front here?
      if buf.output_data_size() == 0 || buf.next_output_data().len() == 0 {
        if self.back_readiness.event.is_readable() {
          self.back_readiness.interest.insert(Ready::readable());
          SessionResult::Continue
        } else {
          SessionResult::CloseSession
        }
      } else {
        self.front_readiness.interest.insert(Ready::writable());
        if self.back_readiness.event.is_readable() {
          self.back_readiness.interest.insert(Ready::readable());
        }
        SessionResult::Continue
      }
    } else {
      SessionResult::CloseSession
    }
  }

  /// Retrieve the response status from the http response state
  pub fn get_response_status(&self) -> Option<RStatusLine> {
    if let Some(state) = self.state.as_ref() {
      state.get_status_line()
    }
    else {
      None
    }
  }

  pub fn get_host(&self) -> Option<String> {
    if let Some(state) = self.state.as_ref() {
      state.get_host()
    }
    else {
      None
    }
  }

  pub fn get_request_line(&self) -> Option<RRequestLine> {
    if let Some(state) = self.state.as_ref() {
      state.get_request_line()
    }
    else {
      None
    }
  }

  pub fn get_session_address(&self) -> Option<SocketAddr> {
    self.session_address.or(self.frontend.socket_ref().peer_addr().ok())
  }

  pub fn get_backend_address(&self) -> Option<SocketAddr> {
    self.backend.as_ref().and_then(|backend| backend.peer_addr().ok())
  }

  pub fn log_request_success(&self, metrics: &SessionMetrics) {
    let session = match self.get_session_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.get_backend_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let host         = self.get_host().unwrap_or(String::from("-"));
    let request_line = self.get_request_line().map(|line| format!("{} {}", line.method, line.uri)).unwrap_or(String::from("-"));
    let status_line  = self.get_response_status().map(|line| format!("{} {}", line.status, line.reason)).unwrap_or(String::from("-"));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    time!("request_time", &app_id, response_time.num_milliseconds());

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(), metrics.backend_bin, metrics.backend_bout);
      }
    }

    info_access!("{}{} -> {}\t{} {} {} {}\t{} {} {}",
      self.log_ctx, session, backend,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      status_line, host, request_line);
  }

  pub fn log_default_answer_success(&self, metrics: &SessionMetrics) {
    let session = match self.get_session_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let status_line = match self.status {
      SessionStatus::Normal => "-",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer301, _, _) => "301 Moved Permanently",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer400, _, _) => "400 Bad Request",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer404, _, _) => "404 Not Found",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer503, _, _) => "503 Service Unavailable",
      SessionStatus::DefaultAnswer(DefaultAnswerStatus::Answer413, _, _) => "413 Payload Too Large",
    };

    let host         = self.get_host().unwrap_or(String::from("-"));
    let request_line = self.get_request_line().map(|line| format!("{} {}", line.method, line.uri)).unwrap_or(String::from("-"));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    if let Some(ref app_id) = self.app_id {
      time!("http.request.time", &app_id, response_time.num_milliseconds());
    }
    incr!("http.errors");

    info_access!("{}{} -> X\t{} {} {} {}\t{} {} {}",
      self.log_ctx, session,
      LogDuration(response_time), LogDuration(service_time),
      metrics.bin, metrics.bout,
      status_line, host, request_line);
  }

  pub fn log_request_error(&mut self, metrics: &mut SessionMetrics, message: &str) {
    metrics.service_stop();
    self.front_readiness.reset();
    self.back_readiness.reset();

    let session = match self.get_session_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let backend = match self.get_backend_address() {
      None => String::from("-"),
      Some(SocketAddr::V4(addr)) => format!("{}", addr),
      Some(SocketAddr::V6(addr)) => format!("{}", addr),
    };

    let host         = self.get_host().unwrap_or(String::from("-"));
    let request_line = self.get_request_line().map(|line| format!("{} {}", line.method, line.uri)).unwrap_or(String::from("-"));
    let status_line  = self.get_response_status().map(|line| format!("{} {}", line.status, line.reason)).unwrap_or(String::from("-"));

    let response_time = metrics.response_time();
    let service_time  = metrics.service_time();

    let app_id = self.app_id.clone().unwrap_or(String::from("-"));
    incr!("http.errors");
    /*time!("request_time", &app_id, response_time);

    if let Some(backend_id) = metrics.backend_id.as_ref() {
      if let Some(backend_response_time) = metrics.backend_response_time() {
        record_backend_metrics!(app_id, backend_id, backend_response_time.num_milliseconds(), metrics.backend_bin, metrics.backend_bout);
      }
    }*/

    error_access!("{}{} -> {}\t{} {} {} {}\t{} {} {} | {}",
      self.log_ctx, session, backend,
      LogDuration(response_time), LogDuration(service_time), metrics.bin, metrics.bout,
      status_line, host, request_line, message);
  }

  // Read content from the session
  pub fn readable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      self.front_readiness.interest.insert(Ready::writable());
      self.back_readiness.interest.remove(Ready::readable());
      self.back_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    assert!(!unwrap_msg!(self.state.as_ref()).is_front_error());

    if self.front_buf.is_none() {
      if let Some(p) = self.pool.upgrade() {
        if let Some(buf) = p.borrow_mut().checkout() {
          self.front_buf = Some(buf);
        } else {
          error!("cannot get front buffer from pool, closing");
          return SessionResult::CloseSession;
        }
      }
    }

    if self.front_buf.as_ref().unwrap().buffer.available_space() == 0 {
      if self.backend_token == None {
        let answer_413 = "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\n\r\n";
        self.set_answer(DefaultAnswerStatus::Answer413, Rc::new(Vec::from(answer_413.as_bytes())));
        self.front_readiness.interest.remove(Ready::readable());
        self.front_readiness.interest.insert(Ready::writable());
      } else {
        self.front_readiness.interest.remove(Ready::readable());
        self.back_readiness.interest.insert(Ready::writable());
      }
      return SessionResult::Continue;
    }

    let (sz, res) = self.frontend.socket_read(self.front_buf.as_mut().unwrap().buffer.space());
    debug!("{}\tFRONT: read {} bytes", self.log_ctx, sz);

    if sz > 0 {
      count!("bytes_in", sz as i64);
      metrics.bin += sz;

      self.front_buf.as_mut().map(|front_buf| {
        front_buf.buffer.fill(sz);
        front_buf.sliced_input(sz);
        if front_buf.start_parsing_position > front_buf.parsed_position {
          let to_consume = min(front_buf.input_data_size(), front_buf.start_parsing_position - front_buf.parsed_position);
          front_buf.consume_parsed_data(to_consume);
        }
      });

      if self.front_buf.as_ref().unwrap().buffer.available_space() == 0 {
        self.front_readiness.interest.remove(Ready::readable());
      }
    } else {
      self.front_readiness.event.remove(Ready::readable());
    }

    match res {
      SocketResult::Error => {
        let front_readiness = self.front_readiness.clone();
        let back_readiness  = self.back_readiness.clone();
        self.log_request_error(metrics,
          &format!("front socket error, closing the connection. Readiness: {:?} -> {:?}", front_readiness, back_readiness));
        return SessionResult::CloseSession;
      },
      SocketResult::Closed => {
        //we were in keep alive but the peer closed the connection
        //FIXME: what happens if the connection was just opened but no data came?
        if unwrap_msg!(self.state.as_ref()).request == Some(RequestState::Initial) {
          metrics.service_stop();
          self.front_readiness.reset();
          self.back_readiness.reset();
          return SessionResult::CloseSession;
        } else {
          let front_readiness = self.front_readiness.clone();
          let back_readiness  = self.back_readiness.clone();
          self.log_request_error(metrics,
            &format!("front socket error, closing the connection. Readiness: {:?} -> {:?}", front_readiness, back_readiness));
          return SessionResult::CloseSession;
        }
      },
      SocketResult::WouldBlock => {
        self.front_readiness.event.remove(Ready::readable());
      },
      SocketResult::Continue => {}
    };

    self.readable_parse(metrics)
  }


  pub fn readable_parse(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    let is_initial = unwrap_msg!(self.state.as_ref()).request == Some(RequestState::Initial);
    // if there's no host, continue parsing until we find it
    let has_host = unwrap_msg!(self.state.as_ref()).has_host();
    if !has_host {
      self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
        &mut self.front_buf.as_mut().unwrap(), &self.sticky_name));
      if unwrap_msg!(self.state.as_ref()).is_front_error() {
        self.log_request_error(metrics, "front parsing error, closing the connection");
        incr!("http.front_parse_errors");

        // increment active requests here because it will be decremented right away
        // when closing the connection. It's slightly easier than decrementing it
        // at every place we return SessionResult::CloseSession
        gauge_add!("http.active_requests", 1);

        return SessionResult::CloseSession;
      }

      let is_now_initial = unwrap_msg!(self.state.as_ref()).request == Some(RequestState::Initial);
      if is_initial && !is_now_initial {
        gauge_add!("http.active_requests", 1);
        incr!("http.requests");
      }

      if unwrap_msg!(self.state.as_ref()).has_host() {
        self.back_readiness.interest.insert(Ready::writable());
        return SessionResult::ConnectBackend;
      } else {
        self.front_readiness.interest.insert(Ready::readable());
        return SessionResult::Continue;
      }
    }

    self.back_readiness.interest.insert(Ready::writable());
    match unwrap_msg!(self.state.as_ref()).request {
      Some(RequestState::Request(_,_,_)) | Some(RequestState::RequestWithBody(_,_,_,_)) => {
        if ! self.front_buf.as_ref().unwrap().needs_input() {
          // stop reading
          self.front_readiness.interest.remove(Ready::readable());
        }
        SessionResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
        error!("{}\tfront read should have stopped on chunk ended", self.log_ctx);
        self.front_readiness.interest.remove(Ready::readable());
        SessionResult::Continue
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Error)) => {
        self.log_request_error(metrics, "front read should have stopped on chunk error");
        SessionResult::CloseSession
      },
      Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
        if ! self.front_buf.as_ref().unwrap().needs_input() {
          self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.front_buf.as_mut().unwrap(), &self.sticky_name));

          if unwrap_msg!(self.state.as_ref()).is_front_error() {
            self.log_request_error(metrics, "front chunk parsing error, closing the connection");
            return SessionResult::CloseSession;
          }

          if let Some(&Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended))) = self.state.as_ref().map(|s| &s.request) {
            self.front_readiness.interest.remove(Ready::readable());
          }
        }
        self.back_readiness.interest.insert(Ready::writable());
        SessionResult::Continue
      },
    _ => {
        self.state = Some(parse_request_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.front_buf.as_mut().unwrap(), &self.sticky_name));

        if unwrap_msg!(self.state.as_ref()).is_front_error() {
          self.log_request_error(metrics, "front parsing error, closing the connection");
          return SessionResult::CloseSession;
        }

        if let Some(&Some(RequestState::Request(_,_,_))) = self.state.as_ref().map(|s| &s.request) {
          self.front_readiness.interest.remove(Ready::readable());
        }
        self.back_readiness.interest.insert(Ready::writable());
        SessionResult::Continue
      }
    }
  }

  fn writable_default_answer(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    let res = if let SessionStatus::DefaultAnswer(answer, ref buf, mut index) = self.status {
      let len = buf.len();

      let mut sz = 0usize;
      let mut res = SocketResult::Continue;
      while res == SocketResult::Continue && index < len {
        let (current_sz, current_res) = self.frontend.socket_write(&buf[index..]);
        res = current_res;
        sz += current_sz;
        index += current_sz;
      }

      count!("bytes_out", sz as i64);
      metrics.bout += sz;

      if res != SocketResult::Continue {
        self.front_readiness.event.remove(Ready::writable());
      }

      if index == len {
        metrics.service_stop();
        self.log_default_answer_success(&metrics);
        self.front_readiness.reset();
        self.back_readiness.reset();
        return SessionResult::CloseSession;
      }

      res
    } else {
      return SessionResult::CloseSession;
    };

    if res == SocketResult::Error {
      self.log_request_error(metrics, "error writing default answer to front socket, closing");
      return SessionResult::CloseSession;
    } else {
      return SessionResult::Continue;
    }
  }

  // Forward content to session
  pub fn writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {

    //handle default answers
    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      return self.writable_default_answer(metrics);
    }

    if self.back_buf.is_none() {
      error!("no back buffer to write on the front socket");
      return SessionResult::CloseSession;
    }

    let output_size = self.back_buf.as_ref().unwrap().output_data_size();
    if self.back_buf.as_ref().map(|buf| buf.output_data_size() == 0 || buf.next_output_data().len() == 0).unwrap() {
      self.back_readiness.interest.insert(Ready::readable());
      self.front_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    let mut sz = 0usize;
    let mut res = SocketResult::Continue;
    while res == SocketResult::Continue && self.back_buf.as_ref().unwrap().output_data_size() > 0 {
      // no more data in buffer, stop here
      if self.back_buf.as_ref().unwrap().next_output_data().len() == 0 {
        self.back_readiness.interest.insert(Ready::readable());
        self.front_readiness.interest.remove(Ready::writable());
        count!("bytes_out", sz as i64);
        metrics.bout += sz;
        return SessionResult::Continue;
      }
      let (current_sz, current_res) = self.frontend.socket_write(self.back_buf.as_ref().unwrap().next_output_data());
      res = current_res;
      self.back_buf.as_mut().unwrap().consume_output_data(current_sz);
      sz += current_sz;
    }
    count!("bytes_out", sz as i64);
    metrics.bout += sz;

    if let Some((front,back)) = self.tokens() {
      debug!("{}\tFRONT [{}<-{}]: wrote {} bytes of {}, buffer position {} restart position {}", self.log_ctx, front.0, back.0, sz, output_size, self.back_buf.as_ref().unwrap().buffer_position, self.back_buf.as_ref().unwrap().start_parsing_position);
    }

    match res {
      SocketResult::Error | SocketResult::Closed => {
        self.log_request_error(metrics, "error writing to front socket, closing");
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.front_readiness.event.remove(Ready::writable());
      },
      SocketResult::Continue => {},
    }

    if !self.back_buf.as_ref().unwrap().can_restart_parsing() {
      self.back_readiness.interest.insert(Ready::readable());
      return SessionResult::Continue;
    }

    //handle this case separately as its cumbersome to do from the pattern match
    if let Some(sz) = self.state.as_ref().and_then(|st| st.must_continue()) {
      self.front_readiness.interest.insert(Ready::readable());
      self.front_readiness.interest.remove(Ready::writable());

      if self.front_buf.is_some() {
        // we must now copy the body from front to back
        trace!("100-Continue => copying {} of body from front to back", sz);
        self.front_buf.as_mut().map(|buf| {
          buf.slice_output(sz);
          buf.consume_parsed_data(sz);
        });

        self.state.as_mut().map(|ref mut st| {
          st.response = Some(ResponseState::Initial);
          st.res_header_end = None;
          st.request.as_mut().map(|r| r.get_mut_connection().map(|conn| conn.continues = Continue::None));
        });
        return SessionResult::Continue;
      } else {
        error!("got 100 continue but front buffer was already removed");
        return SessionResult::CloseSession;
      }
    }


    match unwrap_msg!(self.state.as_ref()).response {
      // FIXME: should only restart parsing if we are using keepalive
      Some(ResponseState::Response(_,_))                            |
      Some(ResponseState::ResponseWithBody(_,_,_))                  |
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
        let front_keep_alive = self.state.as_ref().and_then(|st| st.request.as_ref().map(|r| r.should_keep_alive())).unwrap_or(false);
        let back_keep_alive  = self.state.as_ref().and_then(|st| st.response.as_ref().map(|r| r.should_keep_alive())).unwrap_or(false);

        save_http_status_metric(self.get_response_status());

        self.log_request_success(&metrics);
        metrics.reset();
        //FIXME: we could get smarter about this
        // with no keepalive on backend, we could open a new backend ConnectionError
        // with no keepalive on front but keepalive on backend, we could have
        // a pool of connections
        if front_keep_alive && back_keep_alive {
          debug!("{} keep alive front/back", self.log_ctx);
          self.reset();
          self.front_readiness.interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          self.back_readiness.interest  = UnixReady::hup() | UnixReady::error();

          SessionResult::Continue
          //FIXME: issues reusing the backend socket
          //self.back_readiness.interest  = UnixReady::hup() | UnixReady::error();
          //SessionResult::CloseBackend
        } else if front_keep_alive && !back_keep_alive {
          debug!("{} keep alive front", self.log_ctx);
          self.reset();
          self.front_readiness.interest = UnixReady::from(Ready::readable()) | UnixReady::hup() | UnixReady::error();
          self.back_readiness.interest  = UnixReady::hup() | UnixReady::error();
          SessionResult::CloseBackend(self.backend_token.take())
        } else {
          debug!("{} no keep alive", self.log_ctx);
          self.front_readiness.reset();
          self.back_readiness.reset();
          SessionResult::CloseSession
        }
      },
      // restart parsing, since there will be other chunks next
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        self.back_readiness.interest.insert(Ready::readable());
        SessionResult::Continue
      },
      //we're not done parsing the headers
      Some(ResponseState::HasStatusLine(_,_)) |
      Some(ResponseState::HasUpgrade(_,_,_))  |
      Some(ResponseState::HasLength(_,_,_))   => {
        self.back_readiness.interest.insert(Ready::readable());
        SessionResult::Continue
      },
      _ => {
        self.front_readiness.reset();
        self.back_readiness.reset();
        SessionResult::CloseSession
      }
    }
  }

  // Forward content to application
  pub fn back_writable(&mut self, metrics: &mut SessionMetrics) -> SessionResult {
    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      error!("{}\tsending default answer, should not write to back", self.log_ctx);
      self.back_readiness.interest.remove(Ready::writable());
      self.front_readiness.interest.insert(Ready::writable());
      return SessionResult::Continue;
    }

    if self.front_buf.as_ref().map(|buf| buf.output_data_size() == 0 || buf.next_output_data().len() == 0).unwrap() {
      self.front_readiness.interest.insert(Ready::readable());
      self.back_readiness.interest.remove(Ready::writable());
      return SessionResult::Continue;
    }

    let tokens = self.tokens().clone();
    let output_size = self.front_buf.as_ref().unwrap().output_data_size();
    if self.backend.is_none() {
      self.log_request_error(metrics, "back socket not found, closing connection");
      return SessionResult::CloseSession;
    }

    let mut sz = 0usize;
    let mut socket_res = SocketResult::Continue;

    {
      let sock = unwrap_msg!(self.backend.as_mut());
      while socket_res == SocketResult::Continue && self.front_buf.as_ref().unwrap().output_data_size() > 0 {
        // no more data in buffer, stop here
        if self.front_buf.as_ref().unwrap().next_output_data().len() == 0 {
          self.front_readiness.interest.insert(Ready::readable());
          self.back_readiness.interest.remove(Ready::writable());
          metrics.backend_bout += sz;
          return SessionResult::Continue;
        }
        let (current_sz, current_res) = sock.socket_write(self.front_buf.as_ref().unwrap().next_output_data());
        socket_res = current_res;
        self.front_buf.as_mut().unwrap().consume_output_data(current_sz);
        sz += current_sz;
      }
    }

    metrics.backend_bout += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK [{}->{}]: wrote {} bytes of {}", self.log_ctx, front.0, back.0, sz, output_size);
    }
    match socket_res {
      SocketResult::Error | SocketResult::Closed => {
        self.log_request_error(metrics, "back socket write error, closing connection");
        return SessionResult::CloseSession;
      },
      SocketResult::WouldBlock => {
        self.back_readiness.event.remove(Ready::writable());

      },
      SocketResult::Continue => {}
    }

    // FIXME/ should read exactly as much data as needed
    if self.front_buf.as_ref().unwrap().can_restart_parsing() {
      match unwrap_msg!(self.state.as_ref()).request {
        // the entire request was transmitted
        Some(RequestState::Request(_,_,_))                            |
        Some(RequestState::RequestWithBody(_,_,_,_))                  |
        Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Ended)) => {
          // return the buffer to the pool
          // if there's still data in there, keep it for pipelining
          if self.state.as_ref().map(|st| st.must_continue()).is_none() {
            if self.front_buf.as_ref().map(|buf| buf.empty()) == Some(true) {
              self.front_buf = None;
            }
          }
          self.front_readiness.interest.remove(Ready::readable());
          self.back_readiness.interest.insert(Ready::readable());
          self.back_readiness.interest.remove(Ready::writable());
          SessionResult::Continue
        },
        Some(RequestState::RequestWithBodyChunks(_,_,_,Chunk::Initial)) => {
          if self.state.as_ref().map(|st| st.must_continue()).is_none() {
            self.front_readiness.interest.insert(Ready::readable());
            SessionResult::Continue
          } else {
            // wait for the 100 continue response from the backend
            // keep the front buffer
            self.front_readiness.interest.remove(Ready::readable());
            self.back_readiness.interest.insert(Ready::readable());
            self.back_readiness.interest.remove(Ready::writable());
            SessionResult::Continue
          }
        }
        Some(RequestState::RequestWithBodyChunks(_,_,_,_)) => {
          self.front_readiness.interest.insert(Ready::readable());
          SessionResult::Continue
        },
        //we're not done parsing the headers
        Some(RequestState::HasRequestLine(_,_))       |
        Some(RequestState::HasHost(_,_,_))            |
        Some(RequestState::HasLength(_,_,_))          |
        Some(RequestState::HasHostAndLength(_,_,_,_)) => {
          self.front_readiness.interest.insert(Ready::readable());
          SessionResult::Continue
        },
        _ => {
          self.log_request_error(metrics, "invalid state, closing connection");
          SessionResult::CloseSession
        }
      }
    } else {
      self.front_readiness.interest.insert(Ready::readable());
      self.back_readiness.interest.insert(Ready::writable());
      SessionResult::Continue
    }
  }

  // Read content from application
  pub fn back_readable(&mut self, metrics: &mut SessionMetrics) -> (ProtocolResult, SessionResult) {
    if let SessionStatus::DefaultAnswer(_,_,_) = self.status {
      error!("{}\tsending default answer, should not read from back socket", self.log_ctx);
      self.back_readiness.interest.remove(Ready::readable());
      return (ProtocolResult::Continue, SessionResult::Continue);
    }

    if self.back_buf.is_none() {
      if let Some(p) = self.pool.upgrade() {
        if let Some(buf) = p.borrow_mut().checkout() {
          self.back_buf = Some(buf);
        } else {
          error!("cannot get back buffer from pool, closing");
          return (ProtocolResult::Continue, SessionResult::CloseSession);
        }
      }
    }

    if self.back_buf.as_ref().unwrap().buffer.available_space() == 0 {
      self.back_readiness.interest.remove(Ready::readable());
      return (ProtocolResult::Continue, SessionResult::Continue);
    }

    let tokens     = self.tokens().clone();

    if self.backend.is_none() {
      self.log_request_error(metrics, "back socket not found, closing connection");
      return (ProtocolResult::Continue, SessionResult::CloseSession);
    }

    let (sz, r) = {
      let sock = unwrap_msg!(self.backend.as_mut());
      sock.socket_read(&mut self.back_buf.as_mut().unwrap().buffer.space())
    };

    self.back_buf.as_mut().map(|back_buf| {
      back_buf.buffer.fill(sz);
      back_buf.sliced_input(sz);
    });

    metrics.backend_bin += sz;

    if let Some((front,back)) = tokens {
      debug!("{}\tBACK  [{}<-{}]: read {} bytes", self.log_ctx, front.0, back.0, sz);
    }

    if r != SocketResult::Continue || sz == 0 {
      self.back_readiness.event.remove(Ready::readable());
    }

    if r == SocketResult::Error {
      self.log_request_error(metrics, "back socket read error, closing connection");
      return (ProtocolResult::Continue, SessionResult::CloseSession);
    }

    // isolate that here because the "ref protocol" and the self.state = " make borrowing conflicts
    if let Some(&Some(ResponseState::ResponseUpgrade(_,_, ref protocol))) = self.state.as_ref().map(|s| &s.response) {
      debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
      if protocol == "websocket" {
        return (ProtocolResult::Upgrade, SessionResult::Continue);
      } else {
        //FIXME: should we upgrade to a pipe or send an error?
        return (ProtocolResult::Continue, SessionResult::Continue);
      }
    }

    match unwrap_msg!(self.state.as_ref()).response {
      Some(ResponseState::Response(_,_)) => {
        self.log_request_error(metrics, "should not go back in back_readable if the whole response was parsed");
        (ProtocolResult::Continue, SessionResult::CloseSession)
      },
      Some(ResponseState::ResponseWithBody(_,_,_)) => {
        self.front_readiness.interest.insert(Ready::writable());
        if ! self.back_buf.as_ref().unwrap().needs_input() {
          self.back_readiness.interest.remove(Ready::readable());
        }
        (ProtocolResult::Continue, SessionResult::Continue)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended)) => {
        use nom::HexDisplay;
        self.back_readiness.interest.remove(Ready::readable());
        if sz == 0 {
          (ProtocolResult::Continue, SessionResult::Continue)
        } else {
          error!("{}\tback read should have stopped on chunk ended\nstate: {:?}\ndata:{}", self.log_ctx, self.state,
            self.back_buf.as_ref().unwrap().unparsed_data().to_hex(16));
          self.log_request_error(metrics, "back read should have stopped on chunk ended");
          (ProtocolResult::Continue, SessionResult::CloseSession)
        }
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Error)) => {
        self.log_request_error(metrics, "back read should have stopped on chunk error");
        (ProtocolResult::Continue, SessionResult::CloseSession)
      },
      Some(ResponseState::ResponseWithBodyChunks(_,_,_)) => {
        if ! self.back_buf.as_ref().unwrap().needs_input() {
          self.state = Some(parse_response_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
          &mut self.back_buf.as_mut().unwrap(), &self.sticky_name, self.sticky_session.take()));

          if unwrap_msg!(self.state.as_ref()).is_back_error() {
            self.log_request_error(metrics, "back socket chunk parse error, closing connection");
            return (ProtocolResult::Continue, SessionResult::CloseSession);
          }

          if let Some(&Some(ResponseState::ResponseWithBodyChunks(_,_,Chunk::Ended))) = self.state.as_ref().map(|s| &s.response) {
            self.back_readiness.interest.remove(Ready::readable());
          }
        }
        self.front_readiness.interest.insert(Ready::writable());
        (ProtocolResult::Continue, SessionResult::Continue)
      },
      Some(ResponseState::Error(_,_,_,_,_)) => panic!("{}\tback read should have stopped on responsestate error", self.log_ctx),
      _ => {
        self.state = Some(parse_response_until_stop(unwrap_msg!(self.state.take()), &self.request_id,
        &mut self.back_buf.as_mut().unwrap(), &self.sticky_name, self.sticky_session.take()));

        if unwrap_msg!(self.state.as_ref()).is_back_error() {
          self.log_request_error(metrics, "back socket parse error, closing connection");
          return (ProtocolResult::Continue, SessionResult::CloseSession);
        }

        if let Some(ResponseState::Response(_,_)) = unwrap_msg!(self.state.as_ref()).response {
          self.back_readiness.interest.remove(Ready::readable());
        }

        if let Some(&Some(ResponseState::ResponseUpgrade(_,_, ref protocol))) = self.state.as_ref().map(|s| &s.response) {
          debug!("got an upgrade state[{}]: {:?}", line!(), protocol);
          if protocol == "websocket" {
            return (ProtocolResult::Upgrade, SessionResult::Continue);
          } else {
            //FIXME: should we upgrade to a pipe or send an error?
            return (ProtocolResult::Continue, SessionResult::Continue);
          }
        }

        self.front_readiness.interest.insert(Ready::writable());
        (ProtocolResult::Continue, SessionResult::Continue)
      }
    }
  }
}

/// Save the backend http response status code metric
fn save_http_status_metric(rs_status_line : Option<RStatusLine>) {
  if let Some(rs_status_line) = rs_status_line {
    match rs_status_line.status {
      100...199 => { incr!("http.status.1xx"); },
      200...299 => { incr!("http.status.2xx"); },
      300...399 => { incr!("http.status.3xx"); },
      400...499 => { incr!("http.status.4xx"); },
      500...599 => { incr!("http.status.5xx"); },
      _ => { incr!("http.status.other"); }, // http responses with other codes (protocol error)
    }
  }
}
