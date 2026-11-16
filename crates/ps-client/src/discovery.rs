
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ps_discovery::{build_query, parse_response, Found, MDNS_ADDR, MDNS_PORT};
use ps_transport::TcpTransport;

pub const SEARCH_TIME: Duration = Duration::from_millis(1500);
const SEND_AT: [Duration; 3] = [Duration::from_millis(0), Duration::from_millis(250), Duration::from_millis(750)];
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);

pub fn merge(all: &mut Vec<Found>, f: Found) {
    match all.iter_mut().find(|x| x.instance.eq_ignore_ascii_case(&f.instance)) {
        Some(x) => {
            for a in f.addrs {
                if !x.addrs.contains(&a) {
                    x.addrs.push(a);
                }
            }
            if x.secure.is_none() {
                x.secure = f.secure;
                x.pair_open = f.pair_open;
            }
            if x.proto_version.is_none() {
                x.proto_version = f.proto_version;
            }
        }
        None => all.push(f),
    }
}

pub fn search() -> Vec<Found> {
    search_for(SEARCH_TIME, local_interfaces(), SocketAddr::V4(SocketAddrV4::new(MDNS_ADDR, MDNS_PORT)))
}

pub fn search_for(total: Duration, locals: Vec<Option<Ipv4Addr>>, dest: SocketAddr) -> Vec<Found> {
    let id = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) & 0xFFFF) as u16;
    let (tx, rx) = mpsc::channel::<Found>();
    let t0 = Instant::now();
    let mut threads = Vec::new();
    for local in locals {
        let sock = match open_socket(local) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tx = tx.clone();
        threads.push(std::thread::spawn(move || {
            let query = build_query(id);
            let mut sent = 0;
            let mut buf = vec![0u8; 9100];
            loop {
                let el = t0.elapsed();
                if el >= total {
                    break;
                }
                while sent < SEND_AT.len() && el >= SEND_AT[sent] {
                    let _ = sock.send_to(&query, dest);
                    sent += 1;
                }
                let next = SEND_AT.get(sent).copied().unwrap_or(total).min(total);
                let wait = next.saturating_sub(t0.elapsed()).clamp(Duration::from_millis(5), Duration::from_millis(100));
                let _ = sock.set_read_timeout(Some(wait));
                if let Ok((n, _)) = sock.recv_from(&mut buf) {
                    if n >= 2 && u16::from_be_bytes([buf[0], buf[1]]) == id {
                        for f in parse_response(&buf[..n]) {
                            let _ = tx.send(f);
                        }
                    }
                }
            }
        }));
    }
    drop(tx);
    for t in threads {
        let _ = t.join();
    }
    let mut all = Vec::new();
    for f in rx.try_iter() {
        merge(&mut all, f);
    }
    all
}

fn open_socket(local: Option<Ipv4Addr>) -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind((local.unwrap_or(Ipv4Addr::UNSPECIFIED), 0))?;
    sock.set_multicast_ttl_v4(255)?;
    if let Some(a) = local {
        set_multicast_if(&sock, a)?;
    }
    Ok(sock)
}

pub fn connect_found(f: &Found) -> Result<(TcpTransport, SocketAddr), String> {
    let mut erros = Vec::new();
    for a in &f.addrs {
        let sa = SocketAddr::V4(SocketAddrV4::new(*a, f.port));
        match TcpStream::connect_timeout(&sa, CONNECT_TIMEOUT).and_then(TcpTransport::from_stream) {
            Ok(t) => return Ok((t, sa)),
            Err(e) => erros.push(format!("{sa}: {e}")),
        }
    }
    Err(format!("não foi possível conectar a \"{}\" por nenhum dos endereços ({})", f.instance, erros.join("; ")))
}

pub fn format_line(f: &Found) -> String {
    let addrs: Vec<String> = f.addrs.iter().map(|a| format!("{a}:{}", f.port)).collect();
    let v = f.proto_version.map_or("v=?".to_string(), |v| format!("v={v}"));
    let mut l = format!("{}  {}  {}", f.instance, addrs.join(", "), v);
    if let Some(sec) = f.secure {
        l.push_str(&format!("  sec={}", u8::from(sec)));
    }
    if let Some(open) = f.pair_open {
        l.push_str(&format!("  pair={}", if open { "open" } else { "closed" }));
    }
    l
}

#[cfg(windows)]
pub fn local_interfaces() -> Vec<Option<Ipv4Addr>> {
    let v = windows_impl::ipv4_addresses();
    if v.is_empty() {
        vec![None]
    } else {
        v.into_iter().map(Some).collect()
    }
}

#[cfg(not(windows))]
pub fn local_interfaces() -> Vec<Option<Ipv4Addr>> {
    vec![None]
}

#[cfg(windows)]
fn set_multicast_if(sock: &UdpSocket, addr: Ipv4Addr) -> io::Result<()> {
    windows_impl::set_multicast_if(sock, addr)
}

#[cfg(not(windows))]
fn set_multicast_if(_sock: &UdpSocket, _addr: Ipv4Addr) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
mod windows_impl {
    use std::io;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::os::windows::io::AsRawSocket;

    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{setsockopt, AF_INET, IPPROTO_IP, IP_MULTICAST_IF, SOCKADDR_IN, SOCKET};

    const IF_TYPE_LOOPBACK: u32 = 24;

    pub fn ipv4_addresses() -> Vec<Ipv4Addr> {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 16 * 1024;
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..4 {
            buf.resize((size as usize).div_ceil(8), 0);
            let r = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH), &mut size) };
            if r == ERROR_SUCCESS.0 {
                break;
            }
            if r != ERROR_BUFFER_OVERFLOW.0 {
                return Vec::new();
            }
        }
        let mut out: Vec<Ipv4Addr> = Vec::new();
        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            let a = unsafe { &*cur };
            cur = a.Next;
            if a.OperStatus != IfOperStatusUp || a.IfType == IF_TYPE_LOOPBACK {
                continue;
            }
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = unsafe { &*u };
                u = ua.Next;
                let sa = ua.Address.lpSockaddr;
                if sa.is_null() || unsafe { (*sa).sa_family } != AF_INET {
                    continue;
                }
                let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                let raw = unsafe { sin.sin_addr.S_un.S_addr };
                let ip = Ipv4Addr::from(u32::from_be(raw));
                if !ip.is_loopback() && !ip.is_unspecified() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
        }
        out
    }

    pub fn set_multicast_if(sock: &UdpSocket, addr: Ipv4Addr) -> io::Result<()> {
        let bytes = addr.octets();
        let r = unsafe { setsockopt(SOCKET(sock.as_raw_socket() as usize), IPPROTO_IP.0, IP_MULTICAST_IF, Some(&bytes)) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(nome: &str, addrs: &[[u8; 4]], v: Option<u16>) -> Found {
        Found { instance: nome.into(), host: nome.into(), port: 7700, addrs: addrs.iter().map(|a| Ipv4Addr::from(*a)).collect(), proto_version: v, ..Default::default() }
    }

    #[test]
    fn junta_por_nome_sem_repetir_enderecos() {
        let mut all = Vec::new();
        merge(&mut all, f("Pi", &[[192, 168, 0, 11], [192, 168, 0, 13]], None));
        merge(&mut all, f("pi", &[[192, 168, 0, 13], [10, 0, 0, 1]], Some(1)));
        merge(&mut all, f("outro", &[[1, 1, 1, 1]], Some(1)));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].addrs.len(), 3);
        assert_eq!(all[0].addrs[0], Ipv4Addr::new(192, 168, 0, 11));
        assert_eq!(all[0].proto_version, Some(1));
    }

    #[test]
    fn formata_a_linha_da_lista() {
        let l = format_line(&f("pi-stick", &[[192, 168, 0, 11], [192, 168, 0, 13]], Some(1)));
        assert_eq!(l, "pi-stick  192.168.0.11:7700, 192.168.0.13:7700  v=1");
        assert_eq!(format_line(&f("x", &[[1, 2, 3, 4]], None)), "x  1.2.3.4:7700  v=?");
        let mut s = f("pi", &[[10, 0, 0, 2]], Some(1));
        s.secure = Some(true);
        s.pair_open = Some(false);
        assert_eq!(format_line(&s), "pi  10.0.0.2:7700  v=1  sec=1  pair=closed");
    }

    #[cfg(windows)]
    #[test]
    fn lista_interfaces_do_windows_sem_laco() {
        let v = windows_impl::ipv4_addresses();
        assert!(v.iter().all(|a| !a.is_loopback()));
    }

    #[test]
    fn acha_um_anunciante_no_laco() {
        use ps_discovery::{build_legacy_response, parse_query, Announce};
        let srv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = srv.local_addr().unwrap();
        srv.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let ann = Announce { instance: "pi-falso".into(), host: "pi-falso".into(), port: 7799, addrs: vec![Ipv4Addr::new(10, 0, 0, 2)], proto_version: 1, ..Default::default() };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let ann2 = ann.clone();
        let h = std::thread::spawn(move || {
            let mut buf = [0u8; 9100];
            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                if let Ok((n, src)) = srv.recv_from(&mut buf) {
                    if let Some(q) = parse_query(&buf[..n], &ann2) {
                        let _ = srv.send_to(&build_legacy_response(&ann2, &q), src);
                    }
                }
            }
        });
        let t0 = Instant::now();
        let achados = search_for(Duration::from_millis(600), vec![Some(Ipv4Addr::LOCALHOST)], dest);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        h.join().unwrap();
        assert_eq!(achados.len(), 1, "{achados:?}");
        assert_eq!(achados[0].instance, "pi-falso");
        assert_eq!(achados[0].addrs, ann.addrs);
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn sem_ninguem_respondendo_volta_vazio_no_prazo() {
        let mudo = UdpSocket::bind("127.0.0.1:0").unwrap();
        let t0 = Instant::now();
        let achados = search_for(Duration::from_millis(400), vec![Some(Ipv4Addr::LOCALHOST)], mudo.local_addr().unwrap());
        assert!(achados.is_empty());
        let t = t0.elapsed();
        assert!(t >= Duration::from_millis(350) && t < Duration::from_millis(1200), "{t:?}");
    }
}
