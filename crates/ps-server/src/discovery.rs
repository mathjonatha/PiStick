
use std::collections::HashSet;
use std::ffi::CStr;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ps_discovery::{build_goodbye, build_legacy_response, build_response, parse_query, Announce, MDNS_ADDR, MDNS_PORT};

const REFRESH: Duration = Duration::from_secs(5);
const REPEAT: Duration = Duration::from_secs(1);
const UNICAST_PER_SECOND: u32 = 20;
const MCAST_REPLY_GAP: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct PairFlag(pub Arc<dyn Fn() -> bool + Send + Sync>);

impl std::fmt::Debug for PairFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairFlag")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: Option<String>,
    pub port: u16,
    pub mdns_port: u16,
    pub include_loopback: bool,
    pub pair: Option<PairFlag>,
}

impl Config {
    pub fn new(name: Option<String>, port: u16) -> Config {
        Config { name, port, mdns_port: MDNS_PORT, include_loopback: false, pair: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    pub addr: Ipv4Addr,
    pub wireless: bool,
}

pub fn order_addrs(ifaces: &[Iface]) -> Vec<Ipv4Addr> {
    let mut v: Vec<&Iface> = ifaces.iter().collect();
    v.sort_by(|a, b| a.wireless.cmp(&b.wireless).then_with(|| a.name.cmp(&b.name)).then_with(|| a.addr.cmp(&b.addr)));
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for i in v {
        if !out.contains(&i.addr) {
            out.push(i.addr);
        }
    }
    out
}

struct RateWindow {
    start: Instant,
    count: u32,
    max: u32,
}

impl RateWindow {
    fn new(max: u32, now: Instant) -> RateWindow {
        RateWindow { start: now, count: 0, max }
    }

    fn allow(&mut self, now: Instant) -> bool {
        if now.duration_since(self.start) >= Duration::from_secs(1) {
            self.start = now;
            self.count = 0;
        }
        if self.count >= self.max {
            return false;
        }
        self.count += 1;
        true
    }
}

fn last_err() -> io::Error {
    io::Error::last_os_error()
}

fn open_socket(port: u16) -> io::Result<UdpSocket> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(last_err());
    }
    let fail = |fd: libc::c_int| {
        let e = last_err();
        unsafe { libc::close(fd) };
        e
    };
    let on: libc::c_int = 1;
    for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
        let r = unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, opt, &on as *const libc::c_int as *const libc::c_void, std::mem::size_of::<libc::c_int>() as libc::socklen_t) };
        if r != 0 {
            return Err(fail(fd));
        }
    }
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    let r = unsafe { libc::bind(fd, &addr as *const libc::sockaddr_in as *const libc::sockaddr, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t) };
    if r != 0 {
        return Err(fail(fd));
    }
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
}

fn set_multicast_if(sock: &UdpSocket, addr: Ipv4Addr) -> io::Result<()> {
    let a = libc::in_addr { s_addr: u32::from(addr).to_be() };
    let r = unsafe { libc::setsockopt(sock.as_raw_fd(), libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &a as *const libc::in_addr as *const libc::c_void, std::mem::size_of::<libc::in_addr>() as libc::socklen_t) };
    if r != 0 {
        return Err(last_err());
    }
    Ok(())
}

const SYSFS_NET: &str = "/sys/class/net";

const PREFIXOS_VIRTUAIS: [&str; 8] = ["waydroid", "lxcbr", "lxdbr", "docker", "br-", "veth", "virbr", "vnet"];

pub fn virtual_por_nome(nome: &str) -> bool {
    PREFIXOS_VIRTUAIS.iter().any(|p| nome.starts_with(p))
}

pub fn virtual_por_tipo(sysfs: &Path, nome: &str) -> Option<bool> {
    let dir = sysfs.join(nome);
    if !dir.exists() {
        return None;
    }
    if dir.join("bridge").is_dir() {
        return Some(true);
    }
    let real = std::fs::canonicalize(&dir).ok()?;
    let comps: Vec<_> = real.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let sob_virtual = comps.windows(2).any(|w| w[0] == "devices" && w[1] == "virtual");
    if !sob_virtual {
        return Some(false);
    }
    let uevent = std::fs::read_to_string(dir.join("uevent")).unwrap_or_default();
    let rede_de_verdade = uevent.lines().any(|l| matches!(l.trim(), "DEVTYPE=vlan" | "DEVTYPE=bond"));
    Some(!rede_de_verdade)
}

pub fn interface_virtual(sysfs: &Path, nome: &str) -> bool {
    virtual_por_tipo(sysfs, nome).unwrap_or_else(|| virtual_por_nome(nome))
}

pub fn list_interfaces(include_loopback: bool) -> io::Result<Vec<Iface>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(last_err());
    }
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        if unsafe { (*ifa.ifa_addr).sa_family } as libc::c_int != libc::AF_INET {
            continue;
        }
        let flags = ifa.ifa_flags as libc::c_int;
        if flags & libc::IFF_UP == 0 || flags & libc::IFF_RUNNING == 0 {
            continue;
        }
        if flags & libc::IFF_LOOPBACK != 0 && !include_loopback {
            continue;
        }
        let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        if flags & libc::IFF_LOOPBACK == 0 && interface_virtual(Path::new(SYSFS_NET), &name) {
            continue;
        }
        let wireless = std::path::Path::new("/sys/class/net").join(&name).join("wireless").is_dir();
        out.push(Iface { name, addr, wireless });
    }
    unsafe { libc::freeifaddrs(head) };
    Ok(out)
}

static TERM_SIGNAL: AtomicI32 = AtomicI32::new(0);
static ANUNCIANTE_ATIVO: AtomicBool = AtomicBool::new(false);
static DESPEDIDA_ENVIADA: AtomicBool = AtomicBool::new(false);
static FILHOS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static INSTALADO: Once = Once::new();

pub fn register_child(pid: u32) {
    FILHOS.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
}

type TarefaDeEncerramento = Box<dyn FnOnce() + Send>;
static ENCERRAMENTO: Mutex<Option<TarefaDeEncerramento>> = Mutex::new(None);

pub fn set_shutdown_hook(f: impl FnOnce() + Send + 'static) {
    *ENCERRAMENTO.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(f));
}

extern "C" fn on_term(sig: libc::c_int) {
    TERM_SIGNAL.store(sig, Ordering::SeqCst);
    unsafe { libc::signal(sig, libc::SIG_DFL) };
}

pub fn install_term_handler() {
    INSTALADO.call_once(|| {
        for sig in [libc::SIGTERM, libc::SIGINT] {
            unsafe { libc::signal(sig, on_term as extern "C" fn(libc::c_int) as libc::sighandler_t) };
        }
        let r = std::thread::Builder::new().name("encerramento".into()).spawn(|| loop {
            std::thread::sleep(Duration::from_millis(50));
            let sig = TERM_SIGNAL.load(Ordering::SeqCst);
            if sig == 0 {
                continue;
            }
            let tarefa = ENCERRAMENTO.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(t) = tarefa {
                t();
            }
            let filhos = std::mem::take(&mut *FILHOS.lock().unwrap_or_else(|e| e.into_inner()));
            for pid in filhos.into_iter().rev() {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            }
            let limite = Instant::now() + Duration::from_secs(1);
            while ANUNCIANTE_ATIVO.load(Ordering::SeqCst) && !DESPEDIDA_ENVIADA.load(Ordering::SeqCst) && Instant::now() < limite {
                std::thread::sleep(Duration::from_millis(10));
            }
            unsafe { libc::raise(sig) };
            return;
        });
        if let Err(e) = r {
            eprintln!("[psd] sem tratador de encerramento: {e}");
        }
    });
}

pub fn hostname_label() -> String {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let l = h.trim().split('.').next().unwrap_or("").to_string();
    if l.is_empty() {
        "pistick".into()
    } else {
        l
    }
}

pub struct Announcer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    port: u16,
}

impl Announcer {
    pub fn start(cfg: Config) -> io::Result<Announcer> {
        let sock = open_socket(cfg.mdns_port)?;
        sock.set_multicast_ttl_v4(255)?;
        sock.set_multicast_loop_v4(true)?;
        sock.set_read_timeout(Some(Duration::from_millis(100)))?;
        let port = sock.local_addr()?.port();
        let host = hostname_label();
        let instance = cfg.name.clone().unwrap_or_else(|| host.clone());
        let ann = Announce { instance, host, port: cfg.port, addrs: Vec::new(), proto_version: ps_proto::VERSION, secure: cfg.pair.is_some(), pair_open: cfg.pair.as_ref().is_some_and(|p| (p.0)()) };
        ann.validate().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut worker = Worker::new(sock, cfg, ann, port);
        let stop2 = stop.clone();
        let handle = std::thread::Builder::new().name("mdns".into()).spawn(move || worker.run(&stop2))?;
        ANUNCIANTE_ATIVO.store(true, Ordering::SeqCst);
        Ok(Announcer { stop, handle: Some(handle), port })
    }

    pub fn local_port(&self) -> u16 {
        self.port
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Worker {
    sock: UdpSocket,
    cfg: Config,
    ann: Announce,
    group_port: u16,
    ifaces: Vec<Iface>,
    joined: HashSet<Ipv4Addr>,
    logged: HashSet<String>,
    announces: Vec<Instant>,
    last_mcast_reply: Option<Instant>,
    unicast: RateWindow,
}

impl Worker {
    fn new(sock: UdpSocket, cfg: Config, ann: Announce, group_port: u16) -> Worker {
        Worker { sock, cfg, ann, group_port, ifaces: Vec::new(), joined: HashSet::new(), logged: HashSet::new(), announces: Vec::new(), last_mcast_reply: None, unicast: RateWindow::new(UNICAST_PER_SECOND, Instant::now()) }
    }

    fn log_once(&mut self, key: String, msg: String) {
        if self.logged.insert(key) {
            eprintln!("[psd] mDNS: {msg}");
        }
    }

    fn refresh(&mut self, now: Instant) {
        let ifaces = match list_interfaces(self.cfg.include_loopback) {
            Ok(v) => v,
            Err(e) => {
                self.log_once("getifaddrs".into(), format!("não foi possível listar as interfaces: {e}"));
                return;
            }
        };
        let addrs = order_addrs(&ifaces);
        for i in &ifaces {
            if self.joined.insert(i.addr) {
                if let Err(e) = self.sock.join_multicast_v4(&MDNS_ADDR, &i.addr) {
                    self.log_once(format!("join {}", i.addr), format!("não entrou no grupo em {} ({}): {e}", i.name, i.addr));
                }
            }
        }
        self.joined.retain(|a| addrs.contains(a));
        self.ifaces = ifaces;
        if let Some(p) = &self.cfg.pair {
            let aberto = (p.0)();
            if aberto != self.ann.pair_open {
                self.ann.pair_open = aberto;
                self.announces = vec![now, now + REPEAT];
            }
        }
        if addrs != self.ann.addrs {
            println!("[psd] mDNS: endereços anunciados: {}", if addrs.is_empty() { "nenhum".to_string() } else { addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(", ") });
            self.ann.addrs = addrs;
            self.announces = vec![now, now + REPEAT];
        }
    }

    fn send_multicast(&mut self, make: fn(&Announce) -> Vec<u8>) {
        let dest = SocketAddr::V4(SocketAddrV4::new(MDNS_ADDR, self.group_port));
        for i in self.ifaces.clone() {
            let ann = Announce { addrs: vec![i.addr], ..self.ann.clone() };
            let pkt = make(&ann);
            let r = set_multicast_if(&self.sock, i.addr).and_then(|()| self.sock.send_to(&pkt, dest).map(|_| ()));
            if let Err(e) = r {
                self.log_once(format!("send {}", i.addr), format!("falha ao enviar por {} ({}): {e}", i.name, i.addr));
            }
        }
    }

    fn handle_packet(&mut self, pkt: &[u8], src: SocketAddr, now: Instant) {
        if self.ann.addrs.is_empty() {
            return;
        }
        let Some(q) = parse_query(pkt, &self.ann) else { return };
        if src.port() == self.cfg.mdns_port {
            if self.last_mcast_reply.is_none_or(|t| now.duration_since(t) >= MCAST_REPLY_GAP) {
                self.last_mcast_reply = Some(now);
                self.send_multicast(build_response);
            }
        } else if self.unicast.allow(now) {
            let resp = build_legacy_response(&self.ann, &q);
            if let Err(e) = self.sock.send_to(&resp, src) {
                self.log_once("unicast".into(), format!("falha ao responder a {src}: {e}"));
            }
        }
    }

    fn run(&mut self, stop: &AtomicBool) {
        let mut next_refresh = Instant::now();
        let mut buf = vec![0u8; 9100];
        let term = loop {
            let sig = TERM_SIGNAL.load(Ordering::SeqCst);
            if stop.load(Ordering::SeqCst) || sig != 0 {
                break sig;
            }
            let now = Instant::now();
            if now >= next_refresh {
                self.refresh(now);
                next_refresh = now + REFRESH;
            }
            if self.announces.first().is_some_and(|t| *t <= now) {
                self.announces.remove(0);
                self.send_multicast(build_response);
            }
            match self.sock.recv_from(&mut buf) {
                Ok((n, src)) => self.handle_packet(&buf[..n], src, Instant::now()),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {}
                Err(e) => {
                    self.log_once("recv".into(), format!("erro de leitura: {e}"));
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        };
        if !self.ann.addrs.is_empty() {
            self.send_multicast(build_goodbye);
        }
        let _ = term;
        DESPEDIDA_ENVIADA.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(name: &str, a: [u8; 4], wireless: bool) -> Iface {
        Iface { name: name.into(), addr: Ipv4Addr::from(a), wireless }
    }

    #[test]
    fn fio_vem_antes_do_wifi() {
        let v = [i("wlan0", [192, 168, 0, 13], true), i("eth0", [192, 168, 0, 11], false)];
        assert_eq!(order_addrs(&v), vec![Ipv4Addr::new(192, 168, 0, 11), Ipv4Addr::new(192, 168, 0, 13)]);
        assert_eq!(order_addrs(&v[..1]), vec![Ipv4Addr::new(192, 168, 0, 13)]);
        assert!(order_addrs(&[]).is_empty());
        let r = [i("eth0", [10, 0, 0, 1], false), i("eth1", [10, 0, 0, 1], false)];
        assert_eq!(order_addrs(&r).len(), 1);
    }

    #[test]
    fn ordem_e_estavel_dentro_do_grupo() {
        let v = [i("wlan1", [10, 0, 0, 3], true), i("eth1", [10, 0, 0, 2], false), i("wlan0", [10, 0, 0, 4], true), i("eth0", [10, 0, 0, 1], false)];
        let o: Vec<u8> = order_addrs(&v).iter().map(|a| a.octets()[3]).collect();
        assert_eq!(o, vec![1, 2, 4, 3]);
    }

    #[test]
    fn limite_de_respostas_por_segundo() {
        let t0 = Instant::now();
        let mut r = RateWindow::new(20, t0);
        assert_eq!((0..25).filter(|_| r.allow(t0)).count(), 20);
        assert!(!r.allow(t0 + Duration::from_millis(900)));
        assert!(r.allow(t0 + Duration::from_millis(1000)));
    }

    #[test]
    fn lista_as_interfaces_do_laco_quando_pedido() {
        let com = list_interfaces(true).unwrap();
        assert!(com.iter().any(|i| i.addr == Ipv4Addr::LOCALHOST));
        let sem = list_interfaces(false).unwrap();
        assert!(sem.iter().all(|i| !i.addr.is_loopback()));
    }

    #[test]
    fn nomes_de_ponte_e_de_interface_virtual() {
        for n in ["waydroid0", "lxcbr0", "lxdbr0", "docker0", "br-3f2a9c1d", "veth12ab", "virbr0", "vnet3"] {
            assert!(virtual_por_nome(n), "{n}");
        }
        for n in ["eth0", "wlan0", "enp3s0", "wlp2s0", "usb0", "bond0", "eth0.10", "lo"] {
            assert!(!virtual_por_nome(n), "{n}");
        }
    }

    fn sysfs_falso() -> std::path::PathBuf {
        use std::os::unix::fs::symlink;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let raiz = std::env::temp_dir().join(format!("pistick-sysfs-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&raiz);
        let net = raiz.join("class/net");
        std::fs::create_dir_all(&net).unwrap();
        let cria = |onde: &str, nome: &str, extra: &dyn Fn(&Path)| {
            let alvo = raiz.join("devices").join(onde).join("net").join(nome);
            std::fs::create_dir_all(&alvo).unwrap();
            extra(&alvo);
            symlink(&alvo, net.join(nome)).unwrap();
        };
        cria("platform/soc/fd580000.ethernet", "eth0", &|_| {});
        cria("platform/soc/mmc", "wlan0", &|d| std::fs::create_dir(d.join("wireless")).unwrap());
        cria("virtual", "waydroid0", &|d| std::fs::create_dir(d.join("bridge")).unwrap());
        cria("virtual", "tun0", &|_| {});
        cria("virtual", "minharede0", &|_| {});
        cria("virtual", "vlan10", &|d| std::fs::write(d.join("uevent"), "DEVTYPE=vlan\nINTERFACE=vlan10\n").unwrap());
        cria("platform/soc/usb", "docker-usb0", &|_| {});
        net
    }

    #[test]
    fn tipo_da_interface_pelo_sysfs_vale_mais_que_o_nome() {
        let net = sysfs_falso();
        for n in ["waydroid0", "tun0", "minharede0"] {
            assert_eq!(virtual_por_tipo(&net, n), Some(true), "{n}");
            assert!(interface_virtual(&net, n), "{n}");
        }
        for n in ["eth0", "wlan0", "vlan10", "docker-usb0"] {
            assert_eq!(virtual_por_tipo(&net, n), Some(false), "{n}");
            assert!(!interface_virtual(&net, n), "{n}");
        }
        assert_eq!(virtual_por_tipo(&net, "lxcbr0"), None);
        assert!(interface_virtual(&net, "lxcbr0"));
        assert!(!interface_virtual(&net, "enp9s0"));
        let _ = std::fs::remove_dir_all(net.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn so_interfaces_reais_entram_na_lista() {
        for i in list_interfaces(false).unwrap() {
            assert!(!interface_virtual(Path::new(SYSFS_NET), &i.name), "{} não deveria ser anunciada", i.name);
        }
    }

    #[test]
    fn hostname_e_um_rotulo() {
        let h = hostname_label();
        assert!(!h.is_empty() && !h.contains('.'));
    }
}
