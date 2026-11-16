//! Eventos do núcleo para a casca e o formato binário do `nativePoll()` (docs/26, decisão 17).
//!
//! O núcleo não chama Java: o que ele tem a dizer à casca entra numa fila ([`Fila`]) e a casca a esvazia a cada quadro
//! de animação com `Nucleo.poll()`, que devolve um `byte[]` com **zero ou mais registros** colados:
//!
//! ```text
//! registro = tipo: u8, tamanho: u32 (LE), carga: [u8; tamanho]
//! texto    = tamanho: u32 (LE), bytes UTF-8
//! ```
//!
//! | tipo | evento | carga |
//! |---|---|---|
//! | 1 | [`Evento::Estado`] | fase `u8`, texto |
//! | 2 | [`Evento::Aparelho`] | nome (texto), seguro `u8`, pareamento aberto `u8` (0 não, 1 sim, 2 desconhecido, nos dois), n `u16`, n textos `endereço:porta` |
//! | 3 | [`Evento::BuscaFim`] | achados `u32` |
//! | 4 | [`Evento::Codigo`] | texto (o código de pareamento, `123 456`) |
//! | 5 | [`Evento::ChaveNova`] | 32 bytes (a chave do cliente recém-criada: a casca a protege e grava) |
//! | 6 | [`Evento::Erro`] | texto |
//! | 7 | [`Evento::Janelas`] | foco `u32` (0 = nenhuma), n `u16`, n vezes (id `u32`, título texto) |
//! | 8 | [`Evento::AreaDeTransferencia`] | texto |
//! | 9 | [`Evento::Vibrar`] | milissegundos `u16` |
//! | 10 | [`Evento::Aviso`] | texto |
//! | 11 | [`Evento::Encerrada`] | motivo `u8` (`GOODBYE_*`: 1 = outro cliente assumiu, 2 = serviço parando), texto (nome de quem assumiu) |
//! | 12 | [`Evento::EstadoDoAparelho`] | sinalizadores `u32` (bits `STATUS_*`), temperatura conhecida `u8` (0 não, 1 sim), temperatura em décimos de grau `i16` (0 se desconhecida) |
//!
//! Tipos desconhecidos são pulados pelo tamanho (a casca de uma versão mais velha ignora o que não conhece).
//! [`decodificar`] é o leitor de referência (a casca Java faz o mesmo); ele nunca entra em pânico: entrada truncada,
//! tamanho que passa do fim, texto que não é UTF-8 ou contagem absurda viram [`ErroFormato`].

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use ps_secure::wipe;

/// Fases do [`Evento::Estado`].
pub mod fase {
    pub const OCIOSO: u8 = 0;
    pub const BUSCANDO: u8 = 1;
    pub const CONECTANDO: u8 = 2;
    /// Esperando a aprovação do pareamento no aparelho (o código já foi mostrado).
    pub const PAREANDO: u8 = 3;
    pub const CONECTADO: u8 = 4;
    pub const DESCONECTADO: u8 = 5;
    pub const ERRO: u8 = 6;
}

mod tipo {
    pub const ESTADO: u8 = 1;
    pub const APARELHO: u8 = 2;
    pub const BUSCA_FIM: u8 = 3;
    pub const CODIGO: u8 = 4;
    pub const CHAVE_NOVA: u8 = 5;
    pub const ERRO: u8 = 6;
    pub const JANELAS: u8 = 7;
    pub const AREA: u8 = 8;
    pub const VIBRAR: u8 = 9;
    pub const AVISO: u8 = 10;
    pub const ENCERRADA: u8 = 11;
    pub const ESTADO_DO_APARELHO: u8 = 12;
}

/// Limite de um texto de evento (o título de uma janela remota pode ser qualquer coisa).
const MAX_TEXTO: usize = 64 * 1024;
const MAX_ABAS: usize = 256;
const MAX_ENDERECOS: usize = 32;
/// Eventos pendentes além disto são descartados (a casca parou de esvaziar a fila).
const MAX_FILA: usize = 4096;

/// 32 bytes de segredo que se apagam ao sair da memória e nunca aparecem em `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Segredo(pub [u8; 32]);

impl Drop for Segredo {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl std::fmt::Debug for Segredo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Segredo(..)")
    }
}

/// Algo que o núcleo diz à casca.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evento {
    Estado { fase: u8, texto: String },
    /// Um aparelho achado na rede. `seguro` e `pareamento_aberto` vêm do anúncio mDNS (`None` = não anunciou).
    Aparelho { nome: String, enderecos: Vec<String>, seguro: Option<bool>, pareamento_aberto: Option<bool> },
    BuscaFim { achados: u32 },
    Codigo(String),
    /// A chave vai numa caixa na memória dinâmica: mover o evento copia só o ponteiro, não deixa cópias da chave na
    /// pilha, e o `Drop` do [`Segredo`] a apaga.
    ChaveNova(Box<Segredo>),
    Erro(String),
    /// Janela em foco (0 = nenhuma) e as abas (janelas de nível superior, na ordem de criação).
    Janelas { foco: u32, abas: Vec<(u32, String)> },
    AreaDeTransferencia(String),
    Vibrar(u16),
    Aviso(String),
    /// O aparelho se despediu (`GOODBYE`): a sessão acabou e o núcleo **não** reconecta sozinho com o motivo 1. A casca mostra
    /// o texto (por exemplo, "O cliente X assumiu a sessão").
    Encerrada { motivo: u8, texto: String },
    /// Estado do aparelho (`STATUS`): bits `STATUS_*` (subtensão, temperatura, descartável) e a temperatura em décimos de grau.
    EstadoDoAparelho { sinalizadores: u32, temperatura_dc: Option<i16> },
}

/// Por que um `byte[]` do `nativePoll` não pôde ser lido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErroFormato {
    /// Acabou no meio de um registro ou de um campo.
    Truncado,
    /// Um texto que não é UTF-8.
    Utf8,
    /// Um campo com valor impossível (contagem grande demais, carga com tamanho errado).
    Invalido(&'static str),
}

impl std::fmt::Display for ErroFormato {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErroFormato::Truncado => f.write_str("registro truncado"),
            ErroFormato::Utf8 => f.write_str("texto que não é UTF-8"),
            ErroFormato::Invalido(m) => write!(f, "registro inválido: {m}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Escrita
// ---------------------------------------------------------------------------------------------

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Texto cortado em `MAX_TEXTO` bytes (em fronteira de caractere).
fn put_texto(out: &mut Vec<u8>, t: &str) {
    let mut fim = t.len().min(MAX_TEXTO);
    while !t.is_char_boundary(fim) {
        fim -= 1;
    }
    put_u32(out, fim as u32);
    out.extend_from_slice(&t.as_bytes()[..fim]);
}

fn put_opt_bool(out: &mut Vec<u8>, v: Option<bool>) {
    out.push(match v {
        Some(false) => 0,
        Some(true) => 1,
        None => 2,
    });
}

/// Acrescenta o registro de `e` a `out`.
pub fn codificar(e: &Evento, out: &mut Vec<u8>) {
    let mut carga = Vec::new();
    let t = match e {
        Evento::Estado { fase, texto } => {
            carga.push(*fase);
            put_texto(&mut carga, texto);
            tipo::ESTADO
        }
        Evento::Aparelho { nome, enderecos, seguro, pareamento_aberto } => {
            put_texto(&mut carga, nome);
            put_opt_bool(&mut carga, *seguro);
            put_opt_bool(&mut carga, *pareamento_aberto);
            let n = enderecos.len().min(MAX_ENDERECOS);
            put_u16(&mut carga, n as u16);
            for a in &enderecos[..n] {
                put_texto(&mut carga, a);
            }
            tipo::APARELHO
        }
        Evento::BuscaFim { achados } => {
            put_u32(&mut carga, *achados);
            tipo::BUSCA_FIM
        }
        Evento::Codigo(c) => {
            put_texto(&mut carga, c);
            tipo::CODIGO
        }
        Evento::ChaveNova(k) => {
            carga.extend_from_slice(&k.0);
            tipo::CHAVE_NOVA
        }
        Evento::Erro(m) => {
            put_texto(&mut carga, m);
            tipo::ERRO
        }
        Evento::Janelas { foco, abas } => {
            put_u32(&mut carga, *foco);
            let n = abas.len().min(MAX_ABAS);
            put_u16(&mut carga, n as u16);
            for (id, titulo) in &abas[..n] {
                put_u32(&mut carga, *id);
                put_texto(&mut carga, titulo);
            }
            tipo::JANELAS
        }
        Evento::AreaDeTransferencia(t) => {
            put_texto(&mut carga, t);
            tipo::AREA
        }
        Evento::Vibrar(ms) => {
            put_u16(&mut carga, *ms);
            tipo::VIBRAR
        }
        Evento::Aviso(m) => {
            put_texto(&mut carga, m);
            tipo::AVISO
        }
        Evento::Encerrada { motivo, texto } => {
            carga.push(*motivo);
            put_texto(&mut carga, texto);
            tipo::ENCERRADA
        }
        Evento::EstadoDoAparelho { sinalizadores, temperatura_dc } => {
            put_u32(&mut carga, *sinalizadores);
            carga.push(u8::from(temperatura_dc.is_some()));
            carga.extend_from_slice(&temperatura_dc.unwrap_or(0).to_le_bytes());
            tipo::ESTADO_DO_APARELHO
        }
    };
    out.push(t);
    put_u32(out, carga.len() as u32);
    out.extend_from_slice(&carga);
    // A carga de uma chave não fica solta na memória.
    wipe(&mut carga);
}

// ---------------------------------------------------------------------------------------------
// Leitura
// ---------------------------------------------------------------------------------------------

struct Leitor<'a> {
    b: &'a [u8],
}

impl<'a> Leitor<'a> {
    fn pegar(&mut self, n: usize) -> Result<&'a [u8], ErroFormato> {
        if self.b.len() < n {
            return Err(ErroFormato::Truncado);
        }
        let (a, resto) = self.b.split_at(n);
        self.b = resto;
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8, ErroFormato> {
        Ok(self.pegar(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ErroFormato> {
        let b = self.pegar(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ErroFormato> {
        let b = self.pegar(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn texto(&mut self) -> Result<String, ErroFormato> {
        let n = self.u32()? as usize;
        if n > MAX_TEXTO {
            return Err(ErroFormato::Invalido("texto grande demais"));
        }
        let b = self.pegar(n)?;
        std::str::from_utf8(b).map(str::to_string).map_err(|_| ErroFormato::Utf8)
    }

    fn opt_bool(&mut self) -> Result<Option<bool>, ErroFormato> {
        match self.u8()? {
            0 => Ok(Some(false)),
            1 => Ok(Some(true)),
            2 => Ok(None),
            _ => Err(ErroFormato::Invalido("booleano com valor desconhecido")),
        }
    }

    fn vazio(&self) -> Result<(), ErroFormato> {
        if self.b.is_empty() {
            Ok(())
        } else {
            Err(ErroFormato::Invalido("sobra no fim da carga"))
        }
    }
}

fn decodificar_carga(t: u8, carga: &[u8]) -> Result<Option<Evento>, ErroFormato> {
    let mut r = Leitor { b: carga };
    let e = match t {
        tipo::ESTADO => Evento::Estado { fase: r.u8()?, texto: r.texto()? },
        tipo::APARELHO => {
            let nome = r.texto()?;
            let seguro = r.opt_bool()?;
            let pareamento_aberto = r.opt_bool()?;
            let n = r.u16()? as usize;
            if n > MAX_ENDERECOS {
                return Err(ErroFormato::Invalido("endereços demais"));
            }
            let mut enderecos = Vec::with_capacity(n);
            for _ in 0..n {
                enderecos.push(r.texto()?);
            }
            Evento::Aparelho { nome, enderecos, seguro, pareamento_aberto }
        }
        tipo::BUSCA_FIM => Evento::BuscaFim { achados: r.u32()? },
        tipo::CODIGO => Evento::Codigo(r.texto()?),
        tipo::CHAVE_NOVA => {
            let b = r.pegar(32)?;
            let mut k = Box::new(Segredo([0u8; 32]));
            k.0.copy_from_slice(b);
            Evento::ChaveNova(k)
        }
        tipo::ERRO => Evento::Erro(r.texto()?),
        tipo::JANELAS => {
            let foco = r.u32()?;
            let n = r.u16()? as usize;
            if n > MAX_ABAS {
                return Err(ErroFormato::Invalido("abas demais"));
            }
            let mut abas = Vec::with_capacity(n);
            for _ in 0..n {
                let id = r.u32()?;
                abas.push((id, r.texto()?));
            }
            Evento::Janelas { foco, abas }
        }
        tipo::AREA => Evento::AreaDeTransferencia(r.texto()?),
        tipo::VIBRAR => Evento::Vibrar(r.u16()?),
        tipo::AVISO => Evento::Aviso(r.texto()?),
        tipo::ENCERRADA => Evento::Encerrada { motivo: r.u8()?, texto: r.texto()? },
        tipo::ESTADO_DO_APARELHO => {
            let sinalizadores = r.u32()?;
            let conhecida = r.u8()?;
            let b = r.pegar(2)?;
            let t = i16::from_le_bytes([b[0], b[1]]);
            let temperatura_dc = match conhecida {
                0 => None,
                1 => Some(t),
                _ => return Err(ErroFormato::Invalido("temperatura com indicador desconhecido")),
            };
            Evento::EstadoDoAparelho { sinalizadores, temperatura_dc }
        }
        _ => return Ok(None), // tipo desconhecido: pulado
    };
    r.vazio()?;
    Ok(Some(e))
}

/// Lê todos os registros de `bytes`. Tipos desconhecidos são pulados.
pub fn decodificar(bytes: &[u8]) -> Result<Vec<Evento>, ErroFormato> {
    let mut r = Leitor { b: bytes };
    let mut eventos = Vec::new();
    while !r.b.is_empty() {
        let t = r.u8()?;
        let n = r.u32()? as usize;
        let carga = r.pegar(n)?;
        if let Some(e) = decodificar_carga(t, carga)? {
            eventos.push(e);
        }
    }
    Ok(eventos)
}

// ---------------------------------------------------------------------------------------------
// Fila
// ---------------------------------------------------------------------------------------------

/// A fila de eventos entre as threads do núcleo e o `nativePoll`.
#[derive(Default)]
pub struct Fila {
    inner: Mutex<VecDeque<Evento>>,
}

impl Fila {
    pub fn new() -> Fila {
        Fila::default()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<Evento>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Entra um evento. `Janelas` pendente é substituído pelo mais novo (só o último estado importa); com a fila
    /// cheia o evento novo é descartado.
    pub fn empurrar(&self, e: Evento) {
        let mut q = self.lock();
        if matches!(e, Evento::Janelas { .. }) {
            if let Some(velho) = q.iter_mut().find(|x| matches!(x, Evento::Janelas { .. })) {
                *velho = e;
                return;
            }
        }
        if q.len() < MAX_FILA {
            q.push_back(e);
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Tira todos os eventos pendentes, na ordem, no formato do `nativePoll` (vazio se não há nenhum).
    pub fn drenar_bytes(&self) -> Vec<u8> {
        let eventos: Vec<Evento> = self.lock().drain(..).collect();
        // Cada registro é codificado à parte, com capacidade que cabe o registro da chave (37 bytes) sem realocar, e
        // depois copiado para a saída com o tamanho exato: nenhuma realocação deixa cópia solta de uma chave na
        // memória dinâmica. Os registros parciais são apagados.
        let mut partes: Vec<Vec<u8>> = eventos
            .iter()
            .map(|e| {
                let mut b = Vec::with_capacity(64);
                codificar(e, &mut b);
                b
            })
            .collect();
        let mut out = Vec::with_capacity(partes.iter().map(Vec::len).sum());
        for p in &mut partes {
            out.extend_from_slice(p);
            wipe(p);
        }
        out
    }

    /// Tira todos os eventos pendentes sem codificar (testes).
    pub fn drenar(&self) -> Vec<Evento> {
        self.lock().drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exemplos() -> Vec<Evento> {
        vec![
            Evento::Estado { fase: fase::PAREANDO, texto: "Aguardando a aprovação no aparelho".into() },
            Evento::Aparelho { nome: "pi-stick".into(), enderecos: vec!["192.168.0.13:7700".into(), "10.0.0.2:7700".into()], seguro: Some(true), pareamento_aberto: Some(false) },
            Evento::Aparelho { nome: "sem anúncio".into(), enderecos: vec![], seguro: None, pareamento_aberto: None },
            Evento::BuscaFim { achados: 2 },
            Evento::Codigo("123 456".into()),
            Evento::ChaveNova(Box::new(Segredo([0xAB; 32]))),
            Evento::Erro("não foi possível conectar: tempo esgotado ação ç".into()),
            Evento::Janelas { foco: 7, abas: vec![(7, "foot".into()), (9, "Salvar como…".into())] },
            Evento::Janelas { foco: 0, abas: vec![] },
            Evento::AreaDeTransferencia("texto ÃÇ\nlinha 2".into()),
            Evento::Vibrar(30),
            Evento::Aviso("2 caracteres sem tecla foram descartados".into()),
            Evento::Encerrada { motivo: 1, texto: "notebook de ação".into() },
            Evento::Encerrada { motivo: 2, texto: String::new() },
            Evento::EstadoDoAparelho { sinalizadores: 0b1001, temperatura_dc: Some(487) },
            Evento::EstadoDoAparelho { sinalizadores: 0, temperatura_dc: None },
        ]
    }

    #[test]
    fn cada_tipo_de_evento_faz_ida_e_volta() {
        for e in exemplos() {
            let mut b = Vec::new();
            codificar(&e, &mut b);
            assert_eq!(decodificar(&b).unwrap(), vec![e.clone()], "{e:?}");
        }
    }

    #[test]
    fn varios_registros_colados_voltam_na_ordem() {
        let todos = exemplos();
        let mut b = Vec::new();
        for e in &todos {
            codificar(e, &mut b);
        }
        assert_eq!(decodificar(&b).unwrap(), todos);
        assert_eq!(decodificar(&[]).unwrap(), vec![]);
    }

    #[test]
    fn formato_dos_bytes_e_o_documentado() {
        let mut b = Vec::new();
        codificar(&Evento::Vibrar(0x0102), &mut b);
        assert_eq!(b, [9, 2, 0, 0, 0, 0x02, 0x01]);
        let mut b = Vec::new();
        codificar(&Evento::Codigo("ab".into()), &mut b);
        assert_eq!(b, [4, 6, 0, 0, 0, 2, 0, 0, 0, b'a', b'b']);
        let mut b = Vec::new();
        codificar(&Evento::Encerrada { motivo: 1, texto: "ab".into() }, &mut b);
        assert_eq!(b, [11, 7, 0, 0, 0, 1, 2, 0, 0, 0, b'a', b'b']);
        let mut b = Vec::new();
        codificar(&Evento::EstadoDoAparelho { sinalizadores: 5, temperatura_dc: Some(-12) }, &mut b);
        assert_eq!(b, [12, 7, 0, 0, 0, 5, 0, 0, 0, 1, 0xF4, 0xFF]);
        let mut b = Vec::new();
        codificar(&Evento::EstadoDoAparelho { sinalizadores: 0, temperatura_dc: None }, &mut b);
        assert_eq!(b, [12, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // Indicador de temperatura fora de 0 e 1: inválido.
        assert!(decodificar(&[12, 7, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0]).is_err());
    }

    #[test]
    fn tipo_desconhecido_e_pulado_pelo_tamanho() {
        let mut b = vec![200, 3, 0, 0, 0, 1, 2, 3];
        codificar(&Evento::Vibrar(5), &mut b);
        assert_eq!(decodificar(&b).unwrap(), vec![Evento::Vibrar(5)]);
    }

    #[test]
    fn entrada_truncada_em_qualquer_ponto_da_erro_sem_panico() {
        for e in exemplos() {
            let mut b = Vec::new();
            codificar(&e, &mut b);
            for corte in 0..b.len() {
                assert!(decodificar(&b[..corte]).is_err() || corte == 0, "{e:?} cortado em {corte} não podia ler");
            }
        }
    }

    #[test]
    fn entradas_invalidas_dao_erro_sem_panico() {
        // Tamanho que passa do fim.
        assert_eq!(decodificar(&[1, 255, 255, 255, 255]), Err(ErroFormato::Truncado));
        // Texto que não é UTF-8.
        assert_eq!(decodificar(&[6, 5, 0, 0, 0, 1, 0, 0, 0, 0xFF]), Err(ErroFormato::Utf8));
        // Texto gigante declarado.
        let mut b = vec![6, 9, 0, 0, 0];
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        b.extend_from_slice(&[0; 5]);
        assert!(matches!(decodificar(&b), Err(ErroFormato::Invalido(_))));
        // Carga com sobra.
        assert!(matches!(decodificar(&[9, 3, 0, 0, 0, 1, 0, 9]), Err(ErroFormato::Invalido(_))));
        // Chave com tamanho errado.
        assert_eq!(decodificar(&[5, 3, 0, 0, 0, 1, 2, 3]), Err(ErroFormato::Truncado));
        // Booleano impossível e contagens absurdas.
        let mut b = Vec::new();
        codificar(&Evento::Aparelho { nome: "x".into(), enderecos: vec![], seguro: None, pareamento_aberto: None }, &mut b);
        b[5 + 4 + 1] = 9;
        assert!(matches!(decodificar(&b), Err(ErroFormato::Invalido(_))));
        let mut b = vec![7, 8, 0, 0, 0];
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&u16::MAX.to_le_bytes());
        b.extend_from_slice(&[0, 0]);
        assert!(matches!(decodificar(&b), Err(ErroFormato::Invalido(_))));
    }

    #[test]
    fn nenhuma_sequencia_de_bytes_faz_o_leitor_entrar_em_panico() {
        // Varredura determinística (um gerador congruente): bytes aleatórios nunca derrubam o leitor.
        let mut x = 0x1234_5678u32;
        for _ in 0..3000 {
            let n = (x % 64) as usize;
            let mut b = Vec::with_capacity(n);
            for _ in 0..n {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                b.push((x >> 24) as u8);
            }
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let _ = decodificar(&b);
        }
    }

    #[test]
    fn texto_grande_e_cortado_em_fronteira_de_caractere() {
        let longo = "ç".repeat(MAX_TEXTO); // 2 bytes cada: passa do limite
        let mut b = Vec::new();
        codificar(&Evento::Erro(longo), &mut b);
        let Evento::Erro(t) = decodificar(&b).unwrap().remove(0) else { panic!() };
        assert!(t.len() <= MAX_TEXTO && t.chars().all(|c| c == 'ç'));
    }

    #[test]
    fn fila_mantem_a_ordem_troca_janelas_pendente_e_tem_limite() {
        let f = Fila::new();
        f.empurrar(Evento::Vibrar(1));
        f.empurrar(Evento::Janelas { foco: 1, abas: vec![(1, "a".into())] });
        f.empurrar(Evento::Vibrar(2));
        f.empurrar(Evento::Janelas { foco: 2, abas: vec![(2, "b".into())] });
        assert_eq!(f.len(), 3, "só um Janelas pendente");
        let ev = decodificar(&f.drenar_bytes()).unwrap();
        assert_eq!(ev[0], Evento::Vibrar(1));
        assert_eq!(ev[1], Evento::Janelas { foco: 2, abas: vec![(2, "b".into())] });
        assert_eq!(ev[2], Evento::Vibrar(2));
        assert!(f.is_empty() && f.drenar_bytes().is_empty());
        for i in 0..(MAX_FILA + 10) {
            f.empurrar(Evento::BuscaFim { achados: i as u32 });
        }
        assert_eq!(f.len(), MAX_FILA);
    }

    #[test]
    fn segredo_nao_aparece_no_debug() {
        let t = format!("{:?}", Evento::ChaveNova(Box::new(Segredo([0x42; 32]))));
        assert!(!t.contains("66") && !t.contains("0x42"), "{t}");
    }
}
