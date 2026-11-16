//! Descoberta do PiStick na rede local por mDNS (RFC 6762) com DNS-SD (RFC 6763).
//!
//! Biblioteca pura: só monta e lê bytes, não abre soquete e não tem `unsafe`. O servidor (`psd`) e o
//! cliente (`psc`) cuidam da rede. Os pacotes lidos vêm da rede, de qualquer aparelho; por isso o
//! leitor nunca entra em pânico, nunca aloca sem limite e recusa nomes malformados.

#![forbid(unsafe_code)]

use std::net::Ipv4Addr;

/// Nome do serviço anunciado.
pub const SERVICE: &str = "_pistick._tcp.local";
/// Grupo multicast do mDNS.
pub const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
/// Porta do mDNS.
pub const MDNS_PORT: u16 = 5353;

/// Tamanho máximo de pacote aceito pelo leitor.
pub const MAX_PACKET: usize = 9000;
/// Máximo de registros (perguntas e respostas somadas) aceito em um pacote.
pub const MAX_RECORDS: usize = 64;
/// Máximo de saltos de compressão ao ler um nome.
pub const MAX_JUMPS: usize = 16;
/// Tamanho máximo de um nome em bytes na forma de fio (rótulos, tamanhos e o zero final).
pub const MAX_NAME: usize = 255;
/// Tamanho máximo de um rótulo.
pub const MAX_LABEL: usize = 63;

/// TTL dos registros da resposta multicast, em segundos.
pub const TTL_MULTICAST: u32 = 120;
/// TTL dos registros da resposta unicast legada, em segundos.
pub const TTL_LEGACY: u32 = 10;

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const CLASS_ANY: u16 = 255;
const CACHE_FLUSH: u16 = 0x8000;
const FLAGS_RESPONSE: u16 = 0x8400; // QR=1, AA=1

/// O que o servidor anuncia.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Announce {
    /// Rótulo único da instância, por exemplo `pi-stick` (1 a 63 bytes, UTF-8).
    pub instance: String,
    /// Rótulo do nome de máquina, por exemplo `pi-stick` (vira `pi-stick.local`).
    pub host: String,
    /// Porta TCP do serviço.
    pub port: u16,
    /// Endereços da máquina, em ordem de preferência.
    pub addrs: Vec<Ipv4Addr>,
    /// Versão do protocolo PiStick (vai no TXT como `v=N`).
    pub proto_version: u16,
    /// Marco 9A: o aparelho exige canal cifrado e pareamento (vai no TXT como `sec=1`).
    pub secure: bool,
    /// Marco 9A: o primeiro uso está aberto (vai no TXT como `pair=open`; senão `pair=closed`). Só vale com
    /// `secure`.
    pub pair_open: bool,
}

/// Um aparelho achado pelo cliente.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    pub instance: String,
    pub host: String,
    pub port: u16,
    /// Na ordem dos registros A da resposta.
    pub addrs: Vec<Ipv4Addr>,
    pub proto_version: Option<u16>,
    /// `sec=1` no TXT: o aparelho exige canal cifrado (`None` = o TXT não diz).
    pub secure: Option<bool>,
    /// `pair=open|closed` no TXT: primeiro uso aberto ou não (`None` = o TXT não diz).
    pub pair_open: Option<bool>,
}

/// Pergunta recebida pelo servidor que lhe interessa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    /// Seção de perguntas reescrita sem compressão (para repetir na resposta unicast).
    pub questions: Vec<u8>,
    pub qdcount: u16,
}

impl Announce {
    /// Confere os rótulos; os montadores truncam em 63 bytes, mas é melhor recusar antes.
    pub fn validate(&self) -> Result<(), String> {
        for (nome, v) in [("instância", &self.instance), ("máquina", &self.host)] {
            if v.is_empty() || v.len() > MAX_LABEL {
                return Err(format!("o nome da {nome} precisa ter de 1 a {MAX_LABEL} bytes"));
            }
        }
        if self.host.contains('.') {
            return Err("o nome da máquina não pode ter ponto".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Montagem
// ---------------------------------------------------------------------------------------------

/// Rótulos de um nome: cada rótulo são bytes (a instância pode ter ponto ou espaço dentro).
type Labels = Vec<Vec<u8>>;

fn truncate_label(s: &str) -> &[u8] {
    let b = s.as_bytes();
    if b.len() <= MAX_LABEL {
        return b;
    }
    let mut n = MAX_LABEL;
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    &b[..n]
}

fn service_labels() -> Labels {
    SERVICE.split('.').map(|l| l.as_bytes().to_vec()).collect()
}

fn instance_labels(ann: &Announce) -> Labels {
    let mut v = vec![truncate_label(&ann.instance).to_vec()];
    v.extend(service_labels());
    v
}

fn host_labels(ann: &Announce) -> Labels {
    vec![truncate_label(&ann.host).to_vec(), b"local".to_vec()]
}

fn put_name(out: &mut Vec<u8>, labels: &[Vec<u8>]) {
    for l in labels {
        out.push(l.len() as u8);
        out.extend_from_slice(l);
    }
    out.push(0);
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_header(out: &mut Vec<u8>, id: u16, flags: u16, qd: u16, an: u16) {
    put_u16(out, id);
    put_u16(out, flags);
    put_u16(out, qd);
    put_u16(out, an);
    put_u16(out, 0);
    put_u16(out, 0);
}

/// Escreve um registro: nome, tipo, classe, TTL e dados (o tamanho dos dados é calculado).
fn put_record(out: &mut Vec<u8>, name: &[Vec<u8>], rtype: u16, class: u16, ttl: u32, rdata: &[u8]) {
    put_name(out, name);
    put_u16(out, rtype);
    put_u16(out, class);
    put_u32(out, ttl);
    put_u16(out, rdata.len() as u16);
    out.extend_from_slice(rdata);
}

fn records(ann: &Announce, ttl: u32, flush: bool) -> (Vec<u8>, u16) {
    let flush_bit = if flush { CACHE_FLUSH } else { 0 };
    let inst = instance_labels(ann);
    let host = host_labels(ann);
    let mut out = Vec::with_capacity(160);
    let mut n = 0u16;

    // PTR: registro compartilhado, nunca com cache-flush.
    let mut rd = Vec::new();
    put_name(&mut rd, &inst);
    put_record(&mut out, &service_labels(), TYPE_PTR, CLASS_IN, ttl, &rd);
    n += 1;

    // SRV: prioridade 0, peso 0, porta, alvo.
    let mut rd = vec![0, 0, 0, 0];
    put_u16(&mut rd, ann.port);
    put_name(&mut rd, &host);
    put_record(&mut out, &inst, TYPE_SRV, CLASS_IN | flush_bit, ttl, &rd);
    n += 1;

    // TXT: `v=N` e, com canal cifrado (marco 9A), `sec=1` e `pair=open|closed`; cada uma é uma cadeia.
    let mut cadeias = vec![format!("v={}", ann.proto_version)];
    if ann.secure {
        cadeias.push("sec=1".to_string());
        cadeias.push(format!("pair={}", if ann.pair_open { "open" } else { "closed" }));
    }
    let mut rd = Vec::new();
    for c in &cadeias {
        rd.push(c.len() as u8);
        rd.extend_from_slice(c.as_bytes());
    }
    put_record(&mut out, &inst, TYPE_TXT, CLASS_IN | flush_bit, ttl, &rd);
    n += 1;

    for a in &ann.addrs {
        put_record(&mut out, &host, TYPE_A, CLASS_IN | flush_bit, ttl, &a.octets());
        n += 1;
    }
    (out, n)
}

fn build_answer(ann: &Announce, ttl: u32, flush: bool, id: u16, questions: &[u8], qdcount: u16) -> Vec<u8> {
    let (recs, n) = records(ann, ttl, flush);
    let mut out = Vec::with_capacity(12 + questions.len() + recs.len());
    put_header(&mut out, id, FLAGS_RESPONSE, qdcount, n);
    out.extend_from_slice(questions);
    out.extend_from_slice(&recs);
    out
}

/// Pergunta PTR por [`SERVICE`]. `id` é devolvido na resposta unicast.
pub fn build_query(id: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(48);
    put_header(&mut out, id, 0, 1, 0);
    put_name(&mut out, &service_labels());
    put_u16(&mut out, TYPE_PTR);
    put_u16(&mut out, CLASS_IN);
    out
}

/// Resposta multicast: ID 0, sem perguntas, TTL 120 s, cache-flush em SRV, TXT e A.
pub fn build_response(ann: &Announce) -> Vec<u8> {
    build_answer(ann, TTL_MULTICAST, true, 0, &[], 0)
}

/// Resposta unicast legada: mesmo ID, perguntas repetidas, TTL 10 s, sem cache-flush.
pub fn build_legacy_response(ann: &Announce, q: &Query) -> Vec<u8> {
    build_answer(ann, TTL_LEGACY, false, q.id, &q.questions, q.qdcount)
}

/// Despedida: os mesmos registros da resposta multicast com TTL 0.
pub fn build_goodbye(ann: &Announce) -> Vec<u8> {
    build_answer(ann, 0, true, 0, &[], 0)
}

// ---------------------------------------------------------------------------------------------
// Leitura
// ---------------------------------------------------------------------------------------------

fn be16(p: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*p.get(at)?, *p.get(at.checked_add(1)?)?]))
}

fn be32(p: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*p.get(at)?, *p.get(at + 1)?, *p.get(at + 2)?, *p.get(at + 3)?]))
}

/// Lê um nome em `pos`, aceitando compressão. Devolve os rótulos e a posição logo depois do nome
/// no fluxo original (depois do primeiro ponteiro, se houve).
///
/// Recusa: ponteiro que não aponte para trás do próprio ponteiro (isso exclui frente e si mesmo e
/// garante o fim), mais de [`MAX_JUMPS`] saltos, nome acima de [`MAX_NAME`] bytes, rótulo acima de
/// [`MAX_LABEL`] e os tipos de rótulo reservados (bits 0x40 e 0x80 sem serem ponteiro).
fn read_name(p: &[u8], pos: usize) -> Option<(Labels, usize)> {
    let mut labels: Labels = Vec::new();
    let mut cur = pos;
    let mut end: Option<usize> = None;
    let mut jumps = 0;
    let mut wire_len = 1; // o zero final
    loop {
        let b = *p.get(cur)?;
        match b & 0xC0 {
            0x00 => {
                if b == 0 {
                    return Some((labels, end.unwrap_or(cur + 1)));
                }
                let n = b as usize;
                wire_len += n + 1;
                if wire_len > MAX_NAME {
                    return None;
                }
                labels.push(p.get(cur + 1..cur + 1 + n)?.to_vec());
                cur += 1 + n;
            }
            0xC0 => {
                let target = (((b & 0x3F) as usize) << 8) | *p.get(cur + 1)? as usize;
                if target >= cur {
                    return None;
                }
                jumps += 1;
                if jumps > MAX_JUMPS {
                    return None;
                }
                if end.is_none() {
                    end = Some(cur + 2);
                }
                cur = target;
            }
            _ => return None,
        }
    }
}

fn labels_eq(a: &[Vec<u8>], b: &[Vec<u8>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn labels_eq_str(a: &[Vec<u8>], s: &str) -> bool {
    let b: Labels = s.split('.').map(|l| l.as_bytes().to_vec()).collect();
    labels_eq(a, &b)
}

struct Header {
    id: u16,
    flags: u16,
    qd: usize,
    an: usize,
    ns: usize,
    ar: usize,
}

fn read_header(p: &[u8]) -> Option<Header> {
    if p.len() < 12 || p.len() > MAX_PACKET {
        return None;
    }
    let h = Header {
        id: be16(p, 0)?,
        flags: be16(p, 2)?,
        qd: be16(p, 4)? as usize,
        an: be16(p, 6)? as usize,
        ns: be16(p, 8)? as usize,
        ar: be16(p, 10)? as usize,
    };
    if h.qd + h.an + h.ns + h.ar > MAX_RECORDS {
        return None;
    }
    Some(h)
}

/// Lê um pacote recebido pelo servidor. Devolve `Some` se for uma pergunta (QR=0, opcode 0) que
/// peça [`SERVICE`] (PTR ou ANY), o SRV ou TXT da instância, ou o A do nome de máquina.
pub fn parse_query(pkt: &[u8], ann: &Announce) -> Option<Query> {
    let h = read_header(pkt)?;
    if h.flags & 0xF800 != 0 || h.qd == 0 {
        return None;
    }
    let svc = service_labels();
    let inst = instance_labels(ann);
    let host = host_labels(ann);
    let mut pos = 12;
    let mut relevant = false;
    let mut questions = Vec::new();
    for _ in 0..h.qd {
        let (name, next) = read_name(pkt, pos)?;
        let qtype = be16(pkt, next)?;
        let qclass = be16(pkt, next + 2)?;
        pos = next + 4;
        put_name(&mut questions, &name);
        put_u16(&mut questions, qtype);
        put_u16(&mut questions, qclass);
        let class = qclass & 0x7FFF; // o bit alto é QU
        if class != CLASS_IN && class != CLASS_ANY {
            continue;
        }
        let any = qtype == TYPE_ANY;
        if labels_eq(&name, &svc) && (qtype == TYPE_PTR || any)
            || labels_eq(&name, &inst) && (qtype == TYPE_SRV || qtype == TYPE_TXT || any)
            || labels_eq(&name, &host) && (qtype == TYPE_A || any)
        {
            relevant = true;
        }
    }
    relevant.then_some(Query { id: h.id, questions, qdcount: h.qd as u16 })
}

struct Rec {
    name: Labels,
    rtype: u16,
    ttl: u32,
    data: Data,
}

enum Data {
    Ptr(Labels),
    Srv { port: u16, target: Labels },
    Txt(Vec<u8>),
    A(Ipv4Addr),
    Other,
}

fn read_record(p: &[u8], pos: usize) -> Option<(Rec, usize)> {
    let (name, next) = read_name(p, pos)?;
    let rtype = be16(p, next)?;
    let ttl = be32(p, next + 4)?;
    let rdlen = be16(p, next + 8)? as usize;
    let start = next + 10;
    let end = start + rdlen;
    if end > p.len() {
        return None;
    }
    let data = match rtype {
        TYPE_PTR => {
            let (n, e) = read_name(p, start)?;
            if e > end {
                return None;
            }
            Data::Ptr(n)
        }
        TYPE_SRV => {
            if rdlen < 7 {
                return None;
            }
            let port = be16(p, start + 4)?;
            let (target, e) = read_name(p, start + 6)?;
            if e > end {
                return None;
            }
            Data::Srv { port, target }
        }
        TYPE_TXT => Data::Txt(p[start..end].to_vec()),
        TYPE_A => {
            if rdlen != 4 {
                return None;
            }
            Data::A(Ipv4Addr::new(p[start], p[start + 1], p[start + 2], p[start + 3]))
        }
        _ => Data::Other, // tipo desconhecido (AAAA, NSEC...): pula pelo RDLENGTH
    };
    Some((Rec { name, rtype, ttl, data }, end))
}

/// Valor da cadeia `chave=valor` do TXT (chave sem distinguir maiúsculas).
fn txt_value<'a>(txt: &'a [u8], key: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < txt.len() {
        let n = txt[i] as usize;
        let s = txt.get(i + 1..i + 1 + n)?;
        let k = key.as_bytes();
        if s.len() > k.len() && s[..k.len()].eq_ignore_ascii_case(k) && s[k.len()] == b'=' {
            return std::str::from_utf8(&s[k.len() + 1..]).ok();
        }
        i += 1 + n;
    }
    None
}

fn txt_version(txt: &[u8]) -> Option<u16> {
    txt_value(txt, "v")?.parse().ok()
}

fn first_label(l: &[Vec<u8>]) -> String {
    l.first().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
}

/// Lê uma resposta e devolve os aparelhos completos que ela traz (PTR + SRV + A; o TXT é opcional).
/// Registros com TTL 0 (despedida) são ignorados. Pacote malformado devolve lista vazia.
pub fn parse_response(pkt: &[u8]) -> Vec<Found> {
    parse_response_inner(pkt).unwrap_or_default()
}

fn parse_response_inner(pkt: &[u8]) -> Option<Vec<Found>> {
    let h = read_header(pkt)?;
    if h.flags & 0x8000 == 0 || h.flags & 0x7800 != 0 {
        return None;
    }
    let mut pos = 12;
    for _ in 0..h.qd {
        let (_, next) = read_name(pkt, pos)?;
        pos = next.checked_add(4)?;
        if pos > pkt.len() {
            return None;
        }
    }
    let mut recs: Vec<Rec> = Vec::new();
    for _ in 0..h.an + h.ns + h.ar {
        let (r, next) = read_record(pkt, pos)?;
        pos = next;
        recs.push(r);
    }

    let mut out: Vec<Found> = Vec::new();
    let svc_len = service_labels().len();
    for ptr in recs.iter().filter(|r| r.rtype == TYPE_PTR && r.ttl > 0 && labels_eq_str(&r.name, SERVICE)) {
        let Data::Ptr(inst) = &ptr.data else { continue };
        // O nome da instância tem de ser `<rótulo>._pistick._tcp.local`.
        if inst.len() != svc_len + 1 || !labels_eq_str(&inst[1..], SERVICE) {
            continue;
        }
        let Some((port, target)) = recs.iter().filter(|r| r.ttl > 0 && labels_eq(&r.name, inst)).find_map(|r| match &r.data {
            Data::Srv { port, target } => Some((*port, target.clone())),
            _ => None,
        }) else {
            continue;
        };
        let mut addrs: Vec<Ipv4Addr> = Vec::new();
        for r in recs.iter().filter(|r| r.ttl > 0 && labels_eq(&r.name, &target)) {
            if let Data::A(a) = r.data {
                if !addrs.contains(&a) {
                    addrs.push(a);
                }
            }
        }
        if addrs.is_empty() {
            continue;
        }
        let proto_version = recs.iter().filter(|r| r.ttl > 0 && labels_eq(&r.name, inst)).find_map(|r| match &r.data {
            Data::Txt(t) => txt_version(t),
            _ => None,
        });
        let txt = recs.iter().filter(|r| r.ttl > 0 && labels_eq(&r.name, inst)).find_map(|r| match &r.data {
            Data::Txt(t) => Some(t.as_slice()),
            _ => None,
        });
        let secure = txt.and_then(|t| txt_value(t, "sec")).map(|v| v == "1");
        let pair_open = txt.and_then(|t| txt_value(t, "pair")).map(|v| v.eq_ignore_ascii_case("open"));
        let found = Found { instance: first_label(inst), host: first_label(&target), port, addrs, proto_version, secure, pair_open };
        if !out.iter().any(|f| f.instance.eq_ignore_ascii_case(&found.instance)) {
            out.push(found);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
