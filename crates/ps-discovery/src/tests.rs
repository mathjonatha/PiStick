use super::*;

fn ann() -> Announce {
    Announce {
        instance: "pi-stick".into(),
        host: "pi-stick".into(),
        port: 7700,
        addrs: vec![Ipv4Addr::new(192, 168, 0, 11), Ipv4Addr::new(192, 168, 0, 13)],
        proto_version: 1,
        ..Default::default()
    }
}

fn found_of(a: &Announce) -> Found {
    Found { instance: a.instance.clone(), host: a.host.clone(), port: a.port, addrs: a.addrs.clone(), proto_version: Some(a.proto_version), ..Default::default() }
}

/// Pergunta montada à mão: cabeçalho, nome, tipo e classe.
fn question(id: u16, name: &str, qtype: u16, qclass: u16) -> Vec<u8> {
    let mut p = Vec::new();
    put_header(&mut p, id, 0, 1, 0);
    let labels: Labels = name.split('.').map(|l| l.as_bytes().to_vec()).collect();
    put_name(&mut p, &labels);
    put_u16(&mut p, qtype);
    put_u16(&mut p, qclass);
    p
}

// --- ida e volta -----------------------------------------------------------------------------

#[test]
fn resposta_multicast_ida_e_volta() {
    let a = ann();
    let pkt = build_response(&a);
    assert_eq!(&pkt[..4], &[0, 0, 0x84, 0]); // ID 0, QR=1 AA=1
    assert_eq!(be16(&pkt, 4), Some(0)); // sem perguntas
    assert_eq!(be16(&pkt, 6), Some(5)); // PTR, SRV, TXT, 2 A
    assert_eq!(parse_response(&pkt), vec![found_of(&a)]);
}

#[test]
fn resposta_multicast_tem_ttl_120_e_cache_flush_so_onde_deve() {
    let pkt = build_response(&ann());
    let mut pos = 12;
    let mut vistos = Vec::new();
    for _ in 0..5 {
        let (r, next) = read_record(&pkt, pos).unwrap();
        let class = be16(&pkt, pos_class(&pkt, pos)).unwrap();
        vistos.push((r.rtype, class, r.ttl));
        pos = next;
    }
    assert_eq!(pos, pkt.len());
    assert_eq!(vistos[0], (TYPE_PTR, 1, 120)); // PTR: sem cache-flush
    assert_eq!(vistos[1], (TYPE_SRV, 0x8001, 120));
    assert_eq!(vistos[2], (TYPE_TXT, 0x8001, 120));
    assert_eq!(vistos[3], (TYPE_A, 0x8001, 120));
    assert_eq!(vistos[4], (TYPE_A, 0x8001, 120));
}

/// Posição da classe do registro que começa em `pos` (depois do nome e do tipo).
fn pos_class(p: &[u8], pos: usize) -> usize {
    read_name(p, pos).unwrap().1 + 2
}

#[test]
fn resposta_unicast_legada_repete_id_e_pergunta() {
    let a = ann();
    let q = parse_query(&question(0xBEEF, SERVICE, TYPE_PTR, CLASS_IN), &a).expect("é com a gente");
    assert_eq!(q.id, 0xBEEF);
    assert_eq!(q.qdcount, 1);
    let pkt = build_legacy_response(&a, &q);
    assert_eq!(be16(&pkt, 0), Some(0xBEEF));
    assert_eq!(be16(&pkt, 4), Some(1));
    assert_eq!(parse_response(&pkt), vec![found_of(&a)]);
    // TTL 10 e sem cache-flush.
    let (_, mut pos) = read_name(&pkt, 12).unwrap();
    pos += 4;
    for _ in 0..5 {
        let (r, next) = read_record(&pkt, pos).unwrap();
        assert_eq!(r.ttl, 10);
        assert_eq!(be16(&pkt, pos_class(&pkt, pos)).unwrap() & 0x8000, 0);
        pos = next;
    }
}

#[test]
fn despedida_tem_ttl_zero_e_o_cliente_a_ignora() {
    let pkt = build_goodbye(&ann());
    let (r, _) = read_record(&pkt, 12).unwrap();
    assert_eq!(r.ttl, 0);
    assert!(parse_response(&pkt).is_empty());
}

#[test]
fn instancia_com_ponto_espaco_e_utf8() {
    let mut a = ann();
    a.instance = "Pi da sala.2 ação".into();
    let pkt = build_response(&a);
    let f = parse_response(&pkt);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].instance, "Pi da sala.2 ação");
}

#[test]
fn rotulo_longo_e_truncado_em_63_bytes_sem_quebrar_utf8() {
    let mut a = ann();
    a.instance = "ç".repeat(40); // 80 bytes
    assert!(a.validate().is_err());
    let f = parse_response(&build_response(&a));
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].instance, "ç".repeat(31)); // 62 bytes
}

#[test]
fn validate_confere_rotulos() {
    assert!(ann().validate().is_ok());
    let mut a = ann();
    a.instance.clear();
    assert!(a.validate().is_err());
    let mut a = ann();
    a.host = "a.b".into();
    assert!(a.validate().is_err());
}

#[test]
fn sem_txt_a_versao_fica_vazia_e_sem_endereco_nao_ha_aparelho() {
    let a = ann();
    // Resposta sem o TXT: refaz o pacote sem esse registro.
    let inst = instance_labels(&a);
    let host = host_labels(&a);
    let mut recs = Vec::new();
    let mut rd = Vec::new();
    put_name(&mut rd, &inst);
    put_record(&mut recs, &service_labels(), TYPE_PTR, CLASS_IN, 120, &rd);
    let mut rd = vec![0, 0, 0, 0];
    put_u16(&mut rd, 7700);
    put_name(&mut rd, &host);
    put_record(&mut recs, &inst, TYPE_SRV, CLASS_IN, 120, &rd);
    let mut sem_a = Vec::new();
    put_header(&mut sem_a, 0, FLAGS_RESPONSE, 0, 2);
    sem_a.extend_from_slice(&recs);
    assert!(parse_response(&sem_a).is_empty());

    put_record(&mut recs, &host, TYPE_A, CLASS_IN, 120, &[10, 0, 0, 1]);
    let mut com_a = Vec::new();
    put_header(&mut com_a, 0, FLAGS_RESPONSE, 0, 3);
    com_a.extend_from_slice(&recs);
    let f = parse_response(&com_a);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].proto_version, None);
    assert_eq!(f[0].addrs, vec![Ipv4Addr::new(10, 0, 0, 1)]);
}

#[test]
fn enderecos_repetidos_sao_unidos() {
    let mut a = ann();
    a.addrs = vec![Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(5, 6, 7, 8)];
    let f = parse_response(&build_response(&a));
    assert_eq!(f[0].addrs, vec![Ipv4Addr::new(1, 2, 3, 4), Ipv4Addr::new(5, 6, 7, 8)]);
}

// --- parse_query -----------------------------------------------------------------------------

#[test]
fn query_do_proprio_cliente_e_aceita_pelo_servidor() {
    let q = parse_query(&build_query(0x4242), &ann()).unwrap();
    assert_eq!(q.id, 0x4242);
}

#[test]
fn query_por_ptr_any_srv_txt_e_a() {
    let a = ann();
    assert!(parse_query(&question(1, SERVICE, TYPE_PTR, CLASS_IN), &a).is_some());
    assert!(parse_query(&question(1, SERVICE, TYPE_ANY, CLASS_IN), &a).is_some());
    assert!(parse_query(&question(1, "pi-stick._pistick._tcp.local", TYPE_SRV, CLASS_IN), &a).is_some());
    assert!(parse_query(&question(1, "pi-stick._pistick._tcp.local", TYPE_TXT, CLASS_IN), &a).is_some());
    assert!(parse_query(&question(1, "pi-stick.local", TYPE_A, CLASS_IN), &a).is_some());
    // Bit QU e maiúsculas.
    assert!(parse_query(&question(1, "_PiStick._TCP.Local", TYPE_PTR, 0x8001), &a).is_some());
}

#[test]
fn query_que_nao_interessa_e_ignorada() {
    let a = ann();
    assert!(parse_query(&question(1, "_http._tcp.local", TYPE_PTR, CLASS_IN), &a).is_none());
    assert!(parse_query(&question(1, SERVICE, TYPE_A, CLASS_IN), &a).is_none());
    assert!(parse_query(&question(1, "outro.local", TYPE_A, CLASS_IN), &a).is_none());
    assert!(parse_query(&question(1, SERVICE, TYPE_PTR, 3), &a).is_none()); // classe CH
    // Resposta (QR=1) não é pergunta.
    let mut p = question(1, SERVICE, TYPE_PTR, CLASS_IN);
    p[2] = 0x80;
    assert!(parse_query(&p, &a).is_none());
    // Opcode diferente de 0.
    let mut p = question(1, SERVICE, TYPE_PTR, CLASS_IN);
    p[2] = 0x10;
    assert!(parse_query(&p, &a).is_none());
    // Sem perguntas.
    assert!(parse_query(&build_response(&a), &a).is_none());
}

#[test]
fn query_com_varias_perguntas_repete_todas_sem_compressao() {
    let a = ann();
    // Duas perguntas, a segunda com o nome comprimido (aponta para o nome da primeira).
    let mut p = Vec::new();
    put_header(&mut p, 7, 0, 2, 0);
    put_name(&mut p, &[b"x".to_vec(), b"local".to_vec()]);
    put_u16(&mut p, TYPE_A);
    put_u16(&mut p, CLASS_IN);
    p.extend_from_slice(&[0xC0, 12]);
    put_u16(&mut p, TYPE_ANY);
    put_u16(&mut p, CLASS_IN);
    assert!(parse_query(&p, &a).is_none()); // x.local não é nosso

    let mut p = Vec::new();
    put_header(&mut p, 7, 0, 2, 0);
    put_name(&mut p, &[b"x".to_vec(), b"local".to_vec()]);
    put_u16(&mut p, TYPE_A);
    put_u16(&mut p, CLASS_IN);
    put_name(&mut p, &service_labels());
    put_u16(&mut p, TYPE_PTR);
    put_u16(&mut p, CLASS_IN);
    let q = parse_query(&p, &a).unwrap();
    assert_eq!(q.qdcount, 2);
    assert_eq!(q.questions, p[12..].to_vec());
}

// --- compressão e robustez -------------------------------------------------------------------

/// Monta uma resposta com todos os nomes comprimidos e devolve também as posições úteis.
fn resposta_comprimida() -> Vec<u8> {
    let mut p = Vec::new();
    put_header(&mut p, 0, FLAGS_RESPONSE, 0, 4);
    let svc = p.len(); // 12
    put_name(&mut p, &service_labels());
    let local = svc + 1 + 8 + 1 + 4; // posição do rótulo "local"
    // PTR: o nome é o do serviço, escrito logo acima; dados = "foo" + ponteiro para svc.
    put_u16(&mut p, TYPE_PTR);
    put_u16(&mut p, CLASS_IN);
    put_u32(&mut p, 120);
    put_u16(&mut p, 6);
    let inst = p.len();
    p.extend_from_slice(&[3, b'f', b'o', b'o', 0xC0, svc as u8]);
    // SRV: nome = ponteiro para a instância; alvo = "host" + ponteiro para "local".
    p.extend_from_slice(&[0xC0, inst as u8]);
    put_u16(&mut p, TYPE_SRV);
    put_u16(&mut p, CLASS_IN | CACHE_FLUSH);
    put_u32(&mut p, 120);
    put_u16(&mut p, 6 + 7);
    p.extend_from_slice(&[0, 0, 0, 0, 0x1E, 0x14]); // porta 7700
    let host = p.len();
    p.extend_from_slice(&[4, b'h', b'o', b's', b't', 0xC0, local as u8]);
    // TXT.
    p.extend_from_slice(&[0xC0, inst as u8]);
    put_u16(&mut p, TYPE_TXT);
    put_u16(&mut p, CLASS_IN);
    put_u32(&mut p, 120);
    put_u16(&mut p, 4);
    p.extend_from_slice(&[3, b'v', b'=', b'2']);
    // A.
    p.extend_from_slice(&[0xC0, host as u8]);
    put_u16(&mut p, TYPE_A);
    put_u16(&mut p, CLASS_IN);
    put_u32(&mut p, 120);
    put_u16(&mut p, 4);
    p.extend_from_slice(&[10, 1, 2, 3]);
    p
}

#[test]
fn le_nomes_comprimidos() {
    let f = parse_response(&resposta_comprimida());
    assert_eq!(
        f,
        vec![Found { instance: "foo".into(), host: "host".into(), port: 7700, addrs: vec![Ipv4Addr::new(10, 1, 2, 3)], proto_version: Some(2), ..Default::default() }]
    );
}

#[test]
fn nomes_sao_comparados_sem_diferenciar_maiusculas() {
    let mut p = resposta_comprimida();
    // Troca "_pistick" por "_PISTICK" no nome do serviço (os demais apontam para ele).
    p[13..21].copy_from_slice(b"_PISTICK");
    assert_eq!(parse_response(&p).len(), 1);
}

#[test]
fn recusa_ponteiro_para_frente_ou_para_si_mesmo() {
    let mut p = resposta_comprimida();
    // O primeiro ponteiro (nos dados do PTR) fica na posição 47.
    assert_eq!(&p[47..49], &[0xC0, 12]);
    p[48] = 47; // para si mesmo
    assert!(parse_response(&p).is_empty());
    p[48] = 60; // para frente
    assert!(parse_response(&p).is_empty());
}

#[test]
fn ponteiros_em_laco_nao_travam() {
    // Dois ponteiros que apontam um para o outro: o segundo aponta para frente e é recusado.
    let mut p = Vec::new();
    put_header(&mut p, 0, FLAGS_RESPONSE, 1, 0);
    p.extend_from_slice(&[0xC0, 14, 0xC0, 12]);
    put_u16(&mut p, TYPE_PTR);
    put_u16(&mut p, CLASS_IN);
    assert!(parse_response(&p).is_empty());
    assert!(read_name(&p, 12).is_none());
}

#[test]
fn limita_a_16_saltos() {
    // Cadeia de ponteiros, cada um apontando para o anterior; o primeiro leva ao nome real.
    let mut p = vec![0u8; 12];
    p.extend_from_slice(&[1, b'a', 0]); // nome em 12
    let mut anterior = 12usize;
    let mut inicio_cadeia = Vec::new();
    for _ in 0..20 {
        let pos = p.len();
        p.extend_from_slice(&[0xC0 | (anterior >> 8) as u8, anterior as u8]);
        anterior = pos;
        inicio_cadeia.push(pos);
    }
    // 16 saltos ainda passam (o ponteiro 16 da cadeia faz 16 saltos); o 17º falha.
    assert!(read_name(&p, inicio_cadeia[15]).is_some());
    assert!(read_name(&p, inicio_cadeia[16]).is_none());
    assert!(read_name(&p, inicio_cadeia[19]).is_none());
}

#[test]
fn limita_nome_a_255_bytes_e_rotulo_a_63() {
    // 4 rótulos de 62 bytes = 4*63 + 1 = 253: cabe. 5 rótulos de 62 = 316: não cabe.
    let rot = vec![b'a'; 62];
    let nome = |n: usize| {
        let mut v = vec![0u8; 12];
        for _ in 0..n {
            v.push(62);
            v.extend_from_slice(&rot);
        }
        v.push(0);
        v
    };
    assert!(read_name(&nome(4), 12).is_some());
    assert!(read_name(&nome(5), 12).is_none());
    // Rótulo de 64 bytes (0x40) é tipo reservado; 0x80 também.
    let mut v = vec![0u8; 12];
    v.push(64);
    v.extend_from_slice(&[b'a'; 64]);
    v.push(0);
    assert!(read_name(&v, 12).is_none());
    v[12] = 0x80;
    assert!(read_name(&v, 12).is_none());
}

#[test]
fn nome_truncado_e_recusado() {
    let p = [0u8; 12].iter().copied().chain([5, b'a', b'b']).collect::<Vec<u8>>();
    assert!(read_name(&p, 12).is_none());
}

#[test]
fn ignora_tipo_desconhecido_pelo_rdlength() {
    let a = ann();
    let resp = build_response(&a);
    // Insere um registro AAAA (tipo 28) com 16 bytes de dados antes dos demais e ajusta ANCOUNT.
    let mut extra = Vec::new();
    put_record(&mut extra, &host_labels(&a), 28, CLASS_IN, 120, &[0x20; 16]);
    put_record(&mut extra, &host_labels(&a), 4242, CLASS_IN, 120, &[1, 2, 3]);
    let mut p = resp[..12].to_vec();
    p.extend_from_slice(&extra);
    p.extend_from_slice(&resp[12..]);
    p[7] += 2;
    assert_eq!(parse_response(&p), vec![found_of(&a)]);
}

#[test]
fn recusa_pacote_com_mais_de_64_registros_ou_9000_bytes() {
    let a = ann();
    let mut p = build_response(&a);
    p[6] = 0;
    p[7] = 65;
    assert!(parse_response(&p).is_empty());
    let mut p = build_response(&a);
    p[7] = 5;
    assert_eq!(parse_response(&p).len(), 1);
    // Perguntas somam no total.
    let mut q = question(1, SERVICE, TYPE_PTR, CLASS_IN);
    q[5] = 65;
    assert!(parse_query(&q, &a).is_none());
    // Pacote grande demais, mesmo com conteúdo válido no começo.
    let mut grande = build_response(&a);
    grande.resize(MAX_PACKET + 1, 0);
    assert!(parse_response(&grande).is_empty());
    let mut ok = build_response(&a);
    ok.resize(MAX_PACKET, 0);
    assert_eq!(parse_response(&ok).len(), 1);
}

#[test]
fn recusa_rdlength_alem_do_pacote() {
    let a = ann();
    let mut p = build_response(&a);
    let n = p.len();
    p.truncate(n - 2); // corta o último A
    assert!(parse_response(&p).is_empty());
}

#[test]
fn pacote_que_nao_e_resposta_nao_acha_nada() {
    let a = ann();
    assert!(parse_response(&build_query(1)).is_empty());
    assert!(parse_response(&[]).is_empty());
    assert!(parse_response(&[0; 11]).is_empty());
    assert!(parse_query(&[], &a).is_none());
}

#[test]
fn nao_aceita_instancia_fora_do_servico() {
    // PTR de _pistick._tcp.local apontando para um nome de outro serviço.
    let a = ann();
    let mut p = Vec::new();
    put_header(&mut p, 0, FLAGS_RESPONSE, 0, 3);
    let outro: Labels = vec![b"x".to_vec(), b"_http".to_vec(), b"_tcp".to_vec(), b"local".to_vec()];
    let mut rd = Vec::new();
    put_name(&mut rd, &outro);
    put_record(&mut p, &service_labels(), TYPE_PTR, CLASS_IN, 120, &rd);
    let mut rd = vec![0, 0, 0, 0];
    put_u16(&mut rd, 80);
    put_name(&mut rd, &host_labels(&a));
    put_record(&mut p, &outro, TYPE_SRV, CLASS_IN, 120, &rd);
    put_record(&mut p, &host_labels(&a), TYPE_A, CLASS_IN, 120, &[1, 1, 1, 1]);
    assert!(parse_response(&p).is_empty());
}

// --- vetor real do Avahi ---------------------------------------------------------------------

/// Resposta unicast do `avahi-daemon` 0.8 (Raspberry Pi OS Trixie, 2026-11-14) a uma pergunta PTR
/// por `_pistick._tcp.local` vinda de porta efêmera, com `avahi-publish -s "Vetor Avahi"
/// _pistick._tcp 7799 v=1` rodando. Traz compressão de nomes, PTR/TXT/SRV/AAAA/A e TTL 10; a
/// pergunta é repetida (regra do unicast legado).
const AVAHI: &str = "123484000001000500000000085f7069737469636b045f746370056c6f63616c00000c0001c00c000c00010000000a000e0b5665746f72204176616869c00cc031001000010000000a000403763d31c031002100010000000a0011000000001e770870692d737469636bc01ac061001c00010000000a0010280429b850f307f87f9202c0b15e0040c061000100010000000a0004c0a8000b";

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn le_resposta_real_do_avahi() {
    let pkt = hex(AVAHI);
    assert_eq!(
        parse_response(&pkt),
        vec![Found {
            instance: "Vetor Avahi".into(),
            host: "pi-stick".into(),
            port: 7799,
            addrs: vec![Ipv4Addr::new(192, 168, 0, 11)],
            proto_version: Some(1),
            ..Default::default()
        }]
    );
}

// --- robustez: pacotes aleatórios e mutações -------------------------------------------------

struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn exercita(pkt: &[u8], a: &Announce) {
    let _ = parse_response(pkt);
    if let Some(q) = parse_query(pkt, a) {
        // O que sai de uma pergunta tem de voltar a ser um pacote legível.
        let r = build_legacy_response(a, &q);
        assert_eq!(parse_response(&r).len(), 1);
    }
}

#[test]
fn dez_mil_pacotes_aleatorios_nao_derrubam_o_leitor() {
    let a = ann();
    let mut rng = Xorshift(0x9E37_79B9_7F4A_7C15);
    for i in 0..10_000 {
        let len = rng.below(400);
        let mut pkt: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        // Metade dos pacotes começa com um cabeçalho plausível, para ir além da primeira checagem.
        if i % 2 == 0 && pkt.len() >= 12 {
            pkt[2] = if i % 4 == 0 { 0x84 } else { 0x00 };
            pkt[4] = 0;
            pkt[5] = rng.below(3) as u8;
            pkt[6] = 0;
            pkt[7] = rng.below(8) as u8;
            pkt[8] = 0;
            pkt[9] = 0;
            pkt[10] = 0;
            pkt[11] = rng.below(3) as u8;
        }
        exercita(&pkt, &a);
    }
}

#[test]
fn dez_mil_mutacoes_de_pacotes_validos_nao_derrubam_o_leitor() {
    let a = ann();
    let bases: Vec<Vec<u8>> = vec![
        build_response(&a),
        build_goodbye(&a),
        build_query(9),
        question(3, "pi-stick.local", TYPE_A, CLASS_IN),
        resposta_comprimida(),
        hex(AVAHI),
    ];
    let mut rng = Xorshift(0x1234_5678_9ABC_DEF1);
    for i in 0..10_000 {
        let mut pkt = bases[i % bases.len()].clone();
        for _ in 0..1 + rng.below(4) {
            match rng.below(4) {
                0 if !pkt.is_empty() => {
                    let k = rng.below(pkt.len());
                    pkt[k] = rng.next() as u8;
                }
                1 if !pkt.is_empty() => {
                    let k = rng.below(pkt.len());
                    pkt[k] ^= 1 << rng.below(8);
                }
                2 => {
                    let k = rng.below(pkt.len() + 1);
                    pkt.truncate(k);
                }
                _ => {
                    let k = rng.below(pkt.len() + 1);
                    pkt.insert(k, rng.next() as u8);
                }
            }
        }
        exercita(&pkt, &a);
    }
}

#[test]
fn todo_prefixo_de_pacote_valido_e_seguro() {
    let a = ann();
    for base in [build_response(&a), hex(AVAHI), resposta_comprimida(), build_query(1)] {
        for n in 0..=base.len() {
            exercita(&base[..n], &a);
        }
    }
}

#[test]
fn txt_com_canal_cifrado_leva_sec_e_pair_e_o_cliente_le() {
    for (aberto, esperado) in [(true, Some(true)), (false, Some(false))] {
        let a = Announce { secure: true, pair_open: aberto, ..ann() };
        let achados = parse_response(&build_response(&a));
        assert_eq!(achados.len(), 1);
        assert_eq!(achados[0].secure, Some(true));
        assert_eq!(achados[0].pair_open, esperado);
        assert_eq!(achados[0].proto_version, Some(1));
    }
    // Sem `secure`, o TXT só diz a versão e o cliente não presume nada.
    let achados = parse_response(&build_response(&ann()));
    assert_eq!(achados[0].secure, None);
    assert_eq!(achados[0].pair_open, None);
}
