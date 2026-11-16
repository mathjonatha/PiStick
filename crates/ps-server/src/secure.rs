
use std::collections::VecDeque;
use std::io::Read;
use std::net::{IpAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ps_proto::{write_message, Message, AUTH_PAIRED, AUTH_PAIRED_NOW, AUTH_PENDING, AUTH_REFUSED, GOODBYE_REPLACED, GOODBYE_SHUTDOWN};
use ps_secure::canal::{self, MAGIC};
use ps_secure::chaves::{self, fingerprint, pairing_code, sanitize_name, Peer, PeerList};
use ps_secure::{Keypair, SecureConn};
use ps_transport::{with_deadline, Closer, Split, TcpAcceptor, TcpReader, TcpTransport, Transport};

use crate::source::WindowSource;
use crate::{log_end, serve_session_with, ServerOptions, SessionCtl};

pub const FIRST_USE_WINDOW: Duration = Duration::from_secs(600);
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(120);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const UNKNOWN_MAX: usize = 5;
pub const UNKNOWN_PER: Duration = Duration::from_secs(60);
const MAX_HANDLERS: usize = 32;
const MAX_QUEUED: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub first_use_window: Duration,
    pub pair_timeout: Duration,
    pub handshake_timeout: Duration,
    pub unknown_max: usize,
    pub unknown_per: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { first_use_window: FIRST_USE_WINDOW, pair_timeout: PAIR_TIMEOUT, handshake_timeout: HANDSHAKE_TIMEOUT, unknown_max: UNKNOWN_MAX, unknown_per: UNKNOWN_PER }
    }
}

pub fn default_state_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("STATE_DIRECTORY").filter(|d| !d.is_empty()) {
        let s = d.to_string_lossy().into_owned();
        return Some(PathBuf::from(s.split(':').next().unwrap_or("")));
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("pistick"));
    }
    if let Some(h) = std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(h).join(".local").join("state").join("pistick"));
    }
    if let Some(a) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(a).join("PiStick").join("servidor"));
    }
    None
}

struct Pending {
    id: u64,
    key: [u8; 32],
    name: String,
    fp: String,
    code: [u8; 6],
    decision: Option<bool>,
}

struct Inner {
    list: PeerList,
    first_use_until: Option<Instant>,
    window_until: Option<Instant>,
    pending: Option<Pending>,
    next_id: u64,
    attempts: VecDeque<Instant>,
}

struct Active {
    id: u64,
    key: Option<[u8; 32]>,
    closer: Closer,
    ctl: Arc<SessionCtl>,
}

pub const TAKEOVER_GRACE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRequest {
    pub id: u64,
    pub name: String,
    pub fingerprint: String,
    pub code: [u8; 6],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairWait {
    Request(PairRequest),
    Timeout,
    Busy,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Paired,
    PairedNow,
    Pending(u64, [u8; 6]),
    Refused,
    RateLimited,
}

pub struct Security {
    key: Keypair,
    device_name: String,
    insecure: bool,
    dir: PathBuf,
    timing: Timing,
    inner: Mutex<Inner>,
    cv: Condvar,
    active: Mutex<Option<Active>>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl Security {
    pub fn open(dir: &Path, device_name: &str, insecure: bool, log: impl Fn(&str) + Send + Sync + 'static) -> std::io::Result<Arc<Security>> {
        Security::open_with(dir, device_name, insecure, Timing::default(), log)
    }

    pub fn open_with(dir: &Path, device_name: &str, insecure: bool, timing: Timing, log: impl Fn(&str) + Send + Sync + 'static) -> std::io::Result<Arc<Security>> {
        let (key, created) = chaves::load_or_create_key(&dir.join("chave-do-aparelho"))?;
        let (list, avisos) = chaves::load_list(&dir.join("pareados"))?;
        if created {
            log(&format!("chave do aparelho criada; impressão {}", fingerprint(key.public())));
        } else {
            log(&format!("chave do aparelho carregada; impressão {}", fingerprint(key.public())));
        }
        for a in avisos {
            log(&format!("aviso: arquivo pareados: {a}"));
        }
        let first_use_until = list.is_empty().then(|| Instant::now() + timing.first_use_window);
        log(&format!("{} cliente(s) pareado(s){}", list.len(), if first_use_until.is_some() { "; primeiro uso aberto (a lista está vazia)" } else { "" }));
        Ok(Arc::new(Security {
            key,
            device_name: sanitize_name(device_name),
            insecure,
            dir: dir.to_path_buf(),
            timing,
            inner: Mutex::new(Inner { list, first_use_until, window_until: None, pending: None, next_id: 1, attempts: VecDeque::new() }),
            cv: Condvar::new(),
            active: Mutex::new(None),
            log: Box::new(log),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn log(&self, msg: &str) {
        (self.log)(msg);
    }

    pub fn insecure(&self) -> bool {
        self.insecure
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn device_public(&self) -> [u8; 32] {
        *self.key.public()
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }

    pub fn first_use_open(&self) -> bool {
        let g = self.lock();
        Self::first_use_open_locked(&g)
    }

    fn first_use_open_locked(g: &Inner) -> bool {
        g.list.is_empty() && g.first_use_until.is_some_and(|t| Instant::now() < t)
    }

    fn persist(&self, list: &PeerList) -> std::io::Result<()> {
        chaves::save_list(&self.dir.join("pareados"), list)
    }

    pub fn list(&self) -> Vec<(Peer, bool)> {
        let g = self.lock();
        let ativo = self.active.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|a| a.key);
        g.list.peers.iter().map(|p| (p.clone(), ativo.is_some_and(|k| ps_secure::ct_eq(&k, &p.key)))).collect()
    }

    pub fn admit(&self, key: &[u8; 32], name: &str, handshake_hash: &[u8; 32]) -> Admission {
        let now = Instant::now();
        let mut g = self.lock();
        if g.list.find_key(key).is_some() {
            return Admission::Paired;
        }
        while g.attempts.front().is_some_and(|t| now.duration_since(*t) >= self.timing.unknown_per) {
            g.attempts.pop_front();
        }
        if g.attempts.len() >= self.timing.unknown_max {
            return Admission::RateLimited;
        }
        g.attempts.push_back(now);
        let name = sanitize_name(name);
        let fp = fingerprint(key);
        if Self::first_use_open_locked(&g) {
            g.list.upsert(*key, &name, &chaves::today());
            let copia = g.list.clone();
            if let Err(e) = self.persist(&copia) {
                g.list.remove(&fp);
                self.log(&format!("erro: não foi possível gravar a lista de pareados: {e}; cliente \"{name}\" recusado"));
                return Admission::Refused;
            }
            self.log(&format!("primeiro uso: cliente \"{name}\" (impressão {fp}) pareado"));
            return Admission::PairedNow;
        }
        if g.pending.is_none() && g.window_until.is_some_and(|t| now < t) {
            let id = g.next_id;
            g.next_id += 1;
            let code = pairing_code(handshake_hash);
            g.pending = Some(Pending { id, key: *key, name: name.clone(), fp: fp.clone(), code, decision: None });
            g.window_until = None;
            self.cv.notify_all();
            self.log(&format!("pedido de pareamento de \"{name}\" (impressão {fp}) aguardando aprovação"));
            return Admission::Pending(id, code);
        }
        self.log(&format!("cliente recusado: \"{name}\" (impressão {fp}) não está pareado"));
        Admission::Refused
    }

    pub fn wait_decision(&self, id: u64, alive: &dyn Fn() -> bool) -> bool {
        let deadline = Instant::now() + self.timing.pair_timeout;
        let mut g = self.lock();
        let resultado = loop {
            match &g.pending {
                Some(p) if p.id == id => {
                    if let Some(d) = p.decision {
                        break d;
                    }
                }
                _ => break false,
            }
            let now = Instant::now();
            if now >= deadline {
                break false;
            }
            let step = (deadline - now).min(Duration::from_millis(200));
            g = self.cv.wait_timeout(g, step).unwrap_or_else(|e| e.into_inner()).0;
            drop(g);
            let viva = alive();
            g = self.lock();
            if !viva {
                break false;
            }
        };
        if g.pending.as_ref().is_some_and(|p| p.id == id) {
            let p = g.pending.take();
            if !resultado {
                if let Some(p) = p {
                    self.log(&format!("pedido de pareamento de \"{}\" (impressão {}) encerrado sem aprovação", p.name, p.fp));
                }
            }
        }
        self.cv.notify_all();
        resultado
    }

    pub fn pair_window_open(&self) -> bool {
        let g = self.lock();
        g.window_until.is_some_and(|t| Instant::now() < t)
    }

    pub fn open_pair_window(&self, timeout: Duration, alive: &dyn Fn() -> bool) -> PairWait {
        let deadline = Instant::now() + timeout;
        let mut g = self.lock();
        if g.pending.is_some() || g.window_until.is_some_and(|t| Instant::now() < t) {
            return PairWait::Busy;
        }
        g.window_until = Some(deadline);
        self.log("janela de pareamento aberta (psd pair)");
        let r = loop {
            if let Some(p) = &g.pending {
                break PairWait::Request(PairRequest { id: p.id, name: p.name.clone(), fingerprint: p.fp.clone(), code: p.code });
            }
            let now = Instant::now();
            if now >= deadline {
                break PairWait::Timeout;
            }
            let step = (deadline - now).min(Duration::from_millis(200));
            g = self.cv.wait_timeout(g, step).unwrap_or_else(|e| e.into_inner()).0;
            drop(g);
            let viva = alive();
            g = self.lock();
            if !viva {
                break PairWait::Abandoned;
            }
        };
        if g.pending.is_none() {
            g.window_until = None;
        }
        r
    }

    pub fn decide(&self, id: u64, approve: bool) -> Result<String, String> {
        let mut g = self.lock();
        let Some(p) = g.pending.as_mut().filter(|p| p.id == id && p.decision.is_none()) else {
            return Err("o pedido já venceu ou não existe mais".into());
        };
        let (key, name, fp) = (p.key, p.name.clone(), p.fp.clone());
        if approve {
            g.list.upsert(key, &name, &chaves::today());
            let copia = g.list.clone();
            if let Err(e) = self.persist(&copia) {
                g.list.remove(&fp);
                if let Some(p) = g.pending.as_mut() {
                    p.decision = Some(false);
                }
                self.cv.notify_all();
                return Err(format!("não foi possível gravar a lista de pareados: {e}"));
            }
            self.log(&format!("cliente \"{name}\" (impressão {fp}) pareado por aprovação em psd pair"));
        } else {
            self.log(&format!("pareamento de \"{name}\" (impressão {fp}) negado em psd pair"));
        }
        if let Some(p) = g.pending.as_mut() {
            p.decision = Some(approve);
        }
        self.cv.notify_all();
        Ok(name)
    }

    pub fn remove(&self, selector: &str) -> std::io::Result<Vec<Peer>> {
        let mut g = self.lock();
        let mut nova = g.list.clone();
        let fora = nova.remove(selector);
        if fora.is_empty() {
            return Ok(fora);
        }
        self.persist(&nova)?;
        g.list = nova;
        drop(g);
        for p in &fora {
            self.log(&format!("cliente \"{}\" (impressão {}) removido da lista", p.name, fingerprint(&p.key)));
            self.kick(&p.key);
        }
        Ok(fora)
    }

    pub fn reset(&self) -> std::io::Result<usize> {
        let mut g = self.lock();
        let antes = g.list.peers.clone();
        let vazia = PeerList::default();
        self.persist(&vazia)?;
        g.list = vazia;
        g.first_use_until = Some(Instant::now() + self.timing.first_use_window);
        drop(g);
        self.log("lista de pareados apagada; primeiro uso reaberto");
        for p in &antes {
            self.kick(&p.key);
        }
        Ok(antes.len())
    }

    fn kick(&self, key: &[u8; 32]) {
        let a = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = a.as_ref().filter(|a| a.key.is_some_and(|k| ps_secure::ct_eq(&k, key))) {
            a.closer.close();
        }
    }

    fn set_active(&self, active: Active) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
    }

    fn clear_active(&self) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn active_handle(&self) -> Option<(u64, Arc<SessionCtl>, Closer)> {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|a| (a.id, a.ctl.clone(), a.closer.clone()))
    }

    fn still_active(&self, id: u64) -> bool {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|a| a.id == id)
    }

    pub fn session_active(&self) -> bool {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn end_active(&self, reason: u8, text: &str, grace: Duration, except: Option<u64>) -> bool {
        let Some((id, ctl, closer)) = self.active_handle().filter(|h| Some(h.0) != except) else { return false };
        ctl.request_end(reason, text);
        let limite = Instant::now() + grace;
        while self.still_active(id) {
            if Instant::now() >= limite {
                self.log(&format!("a sessão antiga não terminou em {} ms; conexão fechada à força", grace.as_millis()));
                closer.close();
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    pub fn shutdown_session(&self) -> bool {
        self.end_active(GOODBYE_SHUTDOWN, "", TAKEOVER_GRACE, None)
    }
}

struct Admitted {
    ready: Ready,
    takeover: Option<String>,
    token: u64,
}

impl Admitted {
    fn new(ready: Ready, takeover: Option<String>) -> Admitted {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Admitted { ready, takeover, token: NEXT.fetch_add(1, Ordering::SeqCst) }
    }
}

struct Handoff {
    q: Mutex<VecDeque<Admitted>>,
    cv: Condvar,
}

impl Handoff {
    fn new() -> Arc<Handoff> {
        Arc::new(Handoff { q: Mutex::new(VecDeque::new()), cv: Condvar::new() })
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<Admitted>> {
        self.q.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, a: Admitted) -> bool {
        let mut q = self.lock();
        if a.takeover.is_some() {
            q.clear();
        } else if q.len() >= MAX_QUEUED {
            return false;
        }
        q.push_back(a);
        self.cv.notify_all();
        true
    }

    fn pop_wait(&self) -> Admitted {
        let mut q = self.lock();
        loop {
            if let Some(a) = q.pop_front() {
                return a;
            }
            q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn pending_takeover(&self) -> Option<String> {
        self.lock().iter().rev().find_map(|a| a.takeover.clone())
    }
}

enum Ready {
    Plain(TcpTransport),
    Secure { conn: Box<SecureConn<TcpReader, TcpStream>>, key: [u8; 32] },
}

fn is_loopback(ip: Option<IpAddr>) -> bool {
    ip.is_some_and(|i| i.is_loopback())
}

pub fn plaintext_allowed(peer: Option<IpAddr>, insecure: bool) -> bool {
    insecure || is_loopback(peer)
}

fn sniff<const N: usize>(stream: &TcpStream, timeout: Duration) -> Option<[u8; N]> {
    let deadline = Instant::now() + timeout;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut buf = [0u8; N];
    loop {
        match stream.peek(&mut buf) {
            Ok(0) => return None,
            Ok(n) if n >= N => return Some(buf),
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn looks_like_hello(h: &[u8; 12]) -> bool {
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    (14..=4096).contains(&len) && h[4..6] == [1, 0] && &h[8..12] == b"PSTK"
}

fn handle_incoming(stream: TcpStream, sec: &Security, opts: &ServerOptions) -> Option<Admitted> {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "desconhecido".into());
    let transport = TcpTransport::from_stream(stream).ok()?;
    let ip = transport.peer_ip();
    let head: [u8; 4] = sniff(transport.stream(), sec.timing.handshake_timeout)?;
    if &head != MAGIC {
        if plaintext_allowed(ip, sec.insecure) {
            let _ = transport.set_send_queue_limit(opts.send_queue_bytes);
            let hello = sniff::<12>(transport.stream(), sec.timing.handshake_timeout.min(Duration::from_secs(1))).is_some_and(|h| looks_like_hello(&h));
            return Some(Admitted::new(Ready::Plain(transport), hello.then(|| peer.clone())));
        }
        sec.log(&format!("conexão em texto claro de {peer} recusada (sem Noise; use --insecure só para desenvolvimento)"));
        return None;
    }
    let _ = transport.set_send_queue_limit(opts.send_queue_bytes);
    let alive_stream = transport.stream().try_clone().ok()?;
    let Split { mut reader, mut writer, closer } = transport.split().ok()?;
    let (res, expirou) = with_deadline(&closer, sec.timing.handshake_timeout, || {
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic).map_err(canal::ChannelError::Io)?;
        canal::server_handshake(&mut reader, &mut writer, &sec.key, &sec.device_name)
    });
    let (est, info) = match res {
        Ok(v) if !expirou => v,
        Ok(_) => return None,
        Err(e) => {
            sec.log(&format!("aperto de mão com {peer} falhou: {e}"));
            return None;
        }
    };
    let mut conn = SecureConn::new(reader, writer, closer.clone(), est.send, est.recv, peer.clone());
    let name = sanitize_name(&info.name);
    let fp = fingerprint(&info.static_key);
    let send = |conn: &mut SecureConn<TcpReader, TcpStream>, state: u8, code: [u8; 6]| write_message(&mut conn.writer, &Message::AuthStatus { state, code });
    match sec.admit(&info.static_key, &name, &info.handshake_hash) {
        Admission::Paired => {
            send(&mut conn, AUTH_PAIRED, [0; 6]).ok()?;
            sec.log(&format!("cliente \"{name}\" (impressão {fp}) de {peer} autenticado"));
            Some(Admitted::new(Ready::Secure { conn: Box::new(conn), key: info.static_key }, Some(name.clone())))
        }
        Admission::PairedNow => {
            send(&mut conn, AUTH_PAIRED_NOW, [0; 6]).ok()?;
            Some(Admitted::new(Ready::Secure { conn: Box::new(conn), key: info.static_key }, None))
        }
        Admission::Pending(id, code) => {
            send(&mut conn, AUTH_PENDING, code).ok()?;
            let alive = move || {
                let mut b = [0u8; 1];
                match alive_stream.peek(&mut b) {
                    Ok(0) => false,
                    Ok(_) => true,
                    Err(e) => matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted),
                }
            };
            if sec.wait_decision(id, &alive) {
                send(&mut conn, AUTH_PAIRED_NOW, [0; 6]).ok()?;
                Some(Admitted::new(Ready::Secure { conn: Box::new(conn), key: info.static_key }, None))
            } else {
                let _ = send(&mut conn, AUTH_REFUSED, [0; 6]);
                None
            }
        }
        Admission::Refused => {
            let _ = send(&mut conn, AUTH_REFUSED, [0; 6]);
            None
        }
        Admission::RateLimited => {
            sec.log(&format!("muitas tentativas de clientes desconhecidos por minuto; conexão de {peer} fechada sem resposta"));
            None
        }
    }
}

pub fn serve_secure(
    acceptor: &TcpAcceptor,
    opts: &ServerOptions,
    sec: Arc<Security>,
    max_sessions: Option<usize>,
    mut make_source: impl FnMut() -> Box<dyn WindowSource>,
) -> std::io::Result<()> {
    opts.log(&format!("escutando em {} (canal cifrado Noise; texto claro só do laço local{})", ps_transport::Listener::local(acceptor), if sec.insecure { " e de qualquer origem: --insecure" } else { "" }));
    let listener = acceptor.try_clone_listener()?;
    let handoff = Handoff::new();
    let handlers = Arc::new(AtomicUsize::new(0));
    {
        let (sec, opts, handlers, handoff) = (sec.clone(), opts.clone(), handlers.clone(), handoff.clone());
        std::thread::Builder::new().name("aceita".into()).spawn(move || accept_loop(listener, sec, opts, handoff, handlers))?;
    }
    let mut count = 0usize;
    while max_sessions.is_none_or(|m| count < m) {
        let Admitted { ready, token, .. } = handoff.pop_wait();
        let mut source = make_source();
        let ctl = SessionCtl::new();
        let id = token;
        let (end, peer) = match ready {
            Ready::Plain(t) => {
                let peer = t.peer();
                let stream = t.stream().try_clone();
                let closer = Closer::new(move || {
                    if let Ok(s) = &stream {
                        let _ = s.shutdown(std::net::Shutdown::Both);
                    }
                });
                sec.set_active(Active { id, key: None, closer, ctl: ctl.clone() });
                begin_pending_takeover(&handoff, &ctl, opts);
                let end = serve_session_with(t, opts, source.as_mut(), Some(ctl));
                sec.clear_active();
                (end, peer)
            }
            Ready::Secure { conn, key } => {
                let peer = conn.peer.clone();
                sec.set_active(Active { id, key: Some(key), closer: conn.closer.clone(), ctl: ctl.clone() });
                begin_pending_takeover(&handoff, &ctl, opts);
                let end = serve_session_with(*conn, opts, source.as_mut(), Some(ctl));
                sec.clear_active();
                (end, peer)
            }
        };
        log_end(opts, &peer, &end);
        count += 1;
    }
    Ok(())
}

fn begin_pending_takeover(handoff: &Handoff, ctl: &SessionCtl, opts: &ServerOptions) {
    if let Some(quem) = handoff.pending_takeover() {
        opts.log(&format!("troca de hospedeiro: \"{quem}\" já esperava; a sessão que começa vai ser encerrada"));
        ctl.request_end(GOODBYE_REPLACED, &quem);
    }
}

fn accept_loop(listener: std::net::TcpListener, sec: Arc<Security>, opts: ServerOptions, handoff: Arc<Handoff>, handlers: Arc<AtomicUsize>) {
    loop {
        let (stream, _) = match listener.accept() {
            Ok(c) => c,
            Err(e) => {
                opts.log(&format!("erro ao aceitar conexão: {e}"));
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if handlers.fetch_add(1, Ordering::SeqCst) >= MAX_HANDLERS {
            handlers.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (sec, opts, handoff, h2) = (sec.clone(), opts.clone(), handoff.clone(), handlers.clone());
        let r = std::thread::Builder::new().name("conexao".into()).spawn(move || {
            if let Some(admitted) = handle_incoming(stream, &sec, &opts) {
                let (quem, token) = (admitted.takeover.clone(), admitted.token);
                if handoff.push(admitted) {
                    if let Some(quem) = quem {
                        if sec.end_active(GOODBYE_REPLACED, &quem, TAKEOVER_GRACE, Some(token)) {
                            sec.log(&format!("troca de hospedeiro: \"{quem}\" assumiu a sessão"));
                        }
                    }
                }
            }
            h2.fetch_sub(1, Ordering::SeqCst);
        });
        if r.is_err() {
            handlers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texto_claro_so_do_laco_ou_com_insecure() {
        let lo4: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        let lan: IpAddr = "192.168.0.5".parse().unwrap();
        let v6: IpAddr = "fe80::1".parse().unwrap();
        for ip in [lo4, lo6] {
            assert!(plaintext_allowed(Some(ip), false));
        }
        for ip in [lan, v6] {
            assert!(!plaintext_allowed(Some(ip), false));
            assert!(plaintext_allowed(Some(ip), true));
        }
        assert!(!plaintext_allowed(None, false));
    }

    #[test]
    fn sessao_presa_e_fechada_a_forca_depois_do_prazo_e_a_propria_nao_e_tocada() {
        let dir = std::env::temp_dir().join(format!("ps-secure-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sec = Security::open_with(&dir, "teste", false, Timing::default(), |_| {}).unwrap();
        let fechou = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f2 = fechou.clone();
        let closer = Closer::new(move || f2.store(true, Ordering::SeqCst));
        let ctl = SessionCtl::new();
        sec.set_active(Active { id: 7, key: None, closer, ctl: ctl.clone() });
        assert!(!sec.end_active(GOODBYE_REPLACED, "eu", Duration::from_millis(50), Some(7)), "a sessão que a própria conexão abriu não se encerra");
        assert!(!fechou.load(Ordering::SeqCst));
        let t0 = Instant::now();
        assert!(sec.end_active(GOODBYE_REPLACED, "outro", Duration::from_millis(60), Some(8)));
        assert!(t0.elapsed() >= Duration::from_millis(60), "esperou o prazo");
        assert!(fechou.load(Ordering::SeqCst), "passado o prazo a conexão é fechada à força");
        assert_eq!(ctl.take(), Some((GOODBYE_REPLACED, "outro".to_string())));
        fechou.store(false, Ordering::SeqCst);
        let sec2 = sec.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            sec2.clear_active();
        });
        assert!(sec.end_active(GOODBYE_REPLACED, "outro", Duration::from_secs(2), None));
        assert!(!fechou.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
