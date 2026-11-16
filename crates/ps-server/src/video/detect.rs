//! Detecção de região de vídeo (marco 4, veja "Detecção" em `docs/13-marco-4-video.md`).
//!
//! Lógica pura, sem relógio próprio: o instante (`now_ms`, milissegundos desde o início da sessão)
//! vem sempre de quem chama. Há um [`Detector`] por janela.
//!
//! - A cada quadro que a janela envia, [`Detector::on_frame`] recebe quais blocos mudaram. Cada bloco
//!   guarda o histórico dos últimos [`HISTORY_FRAMES`] quadros.
//! - Um bloco é *quente* se mudou em pelo menos [`HOT_MIN_CHANGES`] desses quadros, os quadros cabem
//!   em [`HOT_WINDOW_MS`] e ele mudou num dos [`HOT_RECENT_FRAMES`] últimos.
//! - A janela *entra* em vídeo quando o retângulo envolvente dos blocos quentes é grande o bastante,
//!   está cheio de blocos quentes e o codec de blocos custa caro nele, e isso vale por
//!   [`ENTRY_HOLD_MS`] seguidos com o mesmo retângulo.
//! - Enquanto está em vídeo, [`Detector::on_damage`] registra o dano que cai na região e
//!   [`Detector::poll`] manda *sair* quando passa [`EXIT_QUIET_MS`] sem dano que cubra metade dela.

/// Lado do bloco do codec, em pixels.
pub const BLOCK_PX: u32 = ps_codec::BLOCK as u32;
/// Quantos quadros enviados cada bloco lembra.
pub const HISTORY_FRAMES: usize = 16;
/// Mudanças mínimas, nos últimos [`HISTORY_FRAMES`] quadros, para um bloco ser quente.
pub const HOT_MIN_CHANGES: u32 = 12;
/// Os [`HISTORY_FRAMES`] quadros têm de caber neste prazo.
pub const HOT_WINDOW_MS: u64 = 2000;
/// O bloco quente também tem de ter mudado num destes últimos quadros (o histórico não envelhece
/// sozinho: sem isto uma rolagem breve seguida de quadros de cursor ainda pareceria vídeo).
pub const HOT_RECENT_FRAMES: u32 = 2;
/// Lado mínimo da região, em pixels.
pub const MIN_REGION_PX: u32 = 256;
/// Largura máxima da região, em pixels.
pub const MAX_REGION_W_PX: u32 = 1920;
/// Altura máxima da região, em pixels.
pub const MAX_REGION_H_PX: u32 = 1080;
/// Percentual mínimo de blocos quentes dentro do retângulo.
pub const MIN_HOT_PERCENT: u32 = 80;
/// Custo médio mínimo da janela no codec de blocos, em bytes por bloco.
pub const MIN_BYTES_PER_BLOCK: f64 = 1024.0;
/// Por quanto tempo seguido a condição de entrada tem de valer, com o mesmo retângulo.
pub const ENTRY_HOLD_MS: u64 = 500;
/// Tempo sem dano que cubra metade da região para sair do vídeo.
pub const EXIT_QUIET_MS: u64 = 1000;
/// Percentual da região que o dano tem de cobrir para contar como "movimento".
pub const EXIT_MIN_COVERAGE_PERCENT: u32 = 50;
/// Depois de sair, a janela só volta a entrar depois deste tempo.
pub const REENTRY_COOLDOWN_MS: u64 = 500;
/// Depois de um erro do codificador, nada entra em vídeo por este tempo.
pub const ENCODER_FAILURE_COOLDOWN_MS: u64 = 10_000;

/// Retângulo em blocos de 64×64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRect {
    pub bx: u32,
    pub by: u32,
    pub bw: u32,
    pub bh: u32,
}

impl BlockRect {
    /// O mesmo retângulo em pixels, recortado a uma janela de `win_w`×`win_h` (a borda direita e a
    /// inferior da região podem ser as da janela).
    pub fn to_rect(self, win_w: u32, win_h: u32) -> ps_codec::Rect {
        let (x, y) = (self.bx * BLOCK_PX, self.by * BLOCK_PX);
        let (x1, y1) = (((self.bx + self.bw) * BLOCK_PX).min(win_w), ((self.by + self.bh) * BLOCK_PX).min(win_h));
        ps_codec::Rect::new(x, y, x1.saturating_sub(x), y1.saturating_sub(y))
    }

    pub fn blocks(self) -> u32 {
        self.bw * self.bh
    }
}

/// Por que a janela saiu do vídeo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Nenhum dano dentro da região por [`EXIT_QUIET_MS`].
    NoDamage,
    /// Por [`EXIT_QUIET_MS`] seguidos o dano cobriu menos da metade da região.
    LowCoverage,
}

/// O que o detector manda fazer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start(BlockRect),
    Stop(StopReason),
}

/// Por que [`Detector::entry_rect`] não aceitou nenhum retângulo (só serve ao rastro de depuração).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Motivo {
    /// Menos de [`HISTORY_FRAMES`] quadros no histórico.
    PoucosQuadros(u64),
    /// Custo médio por bloco abaixo de [`MIN_BYTES_PER_BLOCK`].
    CustoBaixo(f64),
    /// Os [`HISTORY_FRAMES`] quadros não cabem em [`HOT_WINDOW_MS`]: o intervalo medido.
    ForaDaJanela(u64),
    NenhumBlocoQuente,
    /// Retângulo menor que [`MIN_REGION_PX`] (largura e altura em pixels).
    Pequeno(u32, u32),
    /// Menos de [`MIN_HOT_PERCENT`]% de blocos quentes: quentes e total.
    PoucosQuentes(u32, u32),
}

impl Motivo {
    fn chave(&self) -> u8 {
        match self {
            Motivo::PoucosQuadros(_) => 0,
            Motivo::CustoBaixo(_) => 1,
            Motivo::ForaDaJanela(_) => 2,
            Motivo::NenhumBlocoQuente => 3,
            Motivo::Pequeno(..) => 4,
            Motivo::PoucosQuentes(..) => 5,
        }
    }
}

impl std::fmt::Display for Motivo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Motivo::PoucosQuadros(n) => write!(f, "menos de {HISTORY_FRAMES} quadros ({n})"),
            Motivo::CustoBaixo(v) => write!(f, "custo por bloco {v:.0} abaixo de {MIN_BYTES_PER_BLOCK:.0}"),
            Motivo::ForaDaJanela(ms) => write!(f, "{HISTORY_FRAMES} quadros em {ms} ms, fora de HOT_WINDOW_MS ({HOT_WINDOW_MS})"),
            Motivo::NenhumBlocoQuente => write!(f, "nenhum bloco quente"),
            Motivo::Pequeno(w, h) => write!(f, "retângulo pequeno {w}x{h} (mínimo {MIN_REGION_PX})"),
            Motivo::PoucosQuentes(q, t) => write!(f, "menos de {MIN_HOT_PERCENT}% quentes ({q} de {t})"),
        }
    }
}

fn fmt_rect(r: BlockRect) -> String {
    format!("{}x{}+{}+{} (blocos)", r.bw, r.bh, r.bx, r.by)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// A condição de entrada vale desde `since_ms` com este retângulo.
    Candidate { rect: BlockRect, since_ms: u64 },
    Active { rect: BlockRect, last_good_ms: u64, last_damage_ms: u64 },
}

/// Detector de vídeo de uma janela.
pub struct Detector {
    /// Blocos por linha, contando o bloco parcial da borda (tamanho da linha de `changed`).
    cols: usize,
    rows: usize,
    /// Tamanho da janela em pixels.
    width: u32,
    height: u32,
    /// Colunas e linhas de blocos que a região pode usar: os blocos parciais da borda entram, a menos
    /// que a dimensão da janela seja ímpar (a região tem largura e altura pares).
    elig_cols: u32,
    elig_rows: u32,
    /// Um bit por quadro, o menos significativo é o mais recente.
    history: Vec<u16>,
    /// Instante dos últimos [`HISTORY_FRAMES`] quadros, em anel.
    times: [u64; HISTORY_FRAMES],
    frames: u64,
    state: State,
    /// Antes deste instante nada entra (depois de sair).
    not_before_ms: u64,
    /// Antes deste instante nada entra (depois de erro do codificador); sobrevive ao `reset`.
    suppressed_until_ms: u64,
    /// Rastro de depuração (`PSD_DEBUG_DETECT=1`): linhas por transição, que a sessão recolhe. `None` = desligado.
    trace: Option<Vec<String>>,
    /// Chave do último motivo registrado no rastro, para registrar só quando muda.
    last_motivo: Option<u8>,
}

impl Detector {
    pub fn new(width: u32, height: u32) -> Self {
        let mut d = Detector {
            cols: 0,
            rows: 0,
            width: 0,
            height: 0,
            elig_cols: 0,
            elig_rows: 0,
            history: Vec::new(),
            times: [0; HISTORY_FRAMES],
            frames: 0,
            state: State::Idle,
            not_before_ms: 0,
            suppressed_until_ms: 0,
            trace: None,
            last_motivo: None,
        };
        d.reset(width, height);
        d
    }

    /// Janela redimensionada (ou recriada): esquece tudo, menos a supressão por erro do codificador.
    pub fn reset(&mut self, width: u32, height: u32) {
        self.cols = width.div_ceil(BLOCK_PX) as usize;
        self.rows = height.div_ceil(BLOCK_PX) as usize;
        self.width = width;
        self.height = height;
        self.elig_cols = if width.is_multiple_of(2) { width.div_ceil(BLOCK_PX) } else { width / BLOCK_PX };
        self.elig_rows = if height.is_multiple_of(2) { height.div_ceil(BLOCK_PX) } else { height / BLOCK_PX };
        self.state = State::Idle;
        self.not_before_ms = 0;
        self.clear_history();
    }

    /// Liga o rastro de depuração: cada transição de estado e cada mudança do motivo de recusa vira uma linha
    /// (veja [`Detector::take_trace`]). Desligado, nada é registrado.
    pub fn enable_trace(&mut self) {
        self.trace.get_or_insert_with(Vec::new);
    }

    /// Recolhe as linhas do rastro registradas desde a última chamada.
    pub fn take_trace(&mut self) -> Vec<String> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
    }

    fn log(&mut self, now_ms: u64, msg: impl FnOnce() -> String) {
        if let Some(t) = &mut self.trace {
            t.push(format!("detecção t={now_ms}ms {}", msg()));
        }
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.history.resize(self.cols * self.rows, 0);
        self.frames = 0;
    }

    /// Blocos por linha dos vetores `changed` (a largura da janela arredondada para cima).
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// O retângulo `r` em pixels, recortado à janela deste detector.
    pub fn to_pixels(&self, r: BlockRect) -> ps_codec::Rect {
        r.to_rect(self.width, self.height)
    }

    /// Recusa um `Start` (por exemplo, outra janela já tem um fluxo): volta a `Idle` sem castigo; a
    /// condição de entrada terá de valer de novo por [`ENTRY_HOLD_MS`].
    pub fn decline(&mut self) {
        if matches!(self.state, State::Active { .. }) {
            self.state = State::Idle;
        }
    }

    /// A região de vídeo em vigor, se houver.
    pub fn region(&self) -> Option<BlockRect> {
        match self.state {
            State::Active { rect, .. } => Some(rect),
            _ => None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.region().is_some()
    }

    /// Um quadro da janela foi enviado em `now_ms`. `changed` tem um valor por bloco, em ordem de
    /// linha (`cols() * linhas`): `true` se o bloco saiu diferente de SKIP, ou, dentro de uma região
    /// de vídeo, se o dano o tocou. `bytes_per_block` é o custo médio da janela no codec de blocos.
    /// Devolve `Start` quando a janela deve entrar em vídeo; depois do `Start` só atualiza o histórico.
    pub fn on_frame(&mut self, now_ms: u64, changed: &[bool], bytes_per_block: f64) -> Option<Action> {
        if changed.len() != self.history.len() {
            return None;
        }
        for (h, &c) in self.history.iter_mut().zip(changed) {
            *h = (*h << 1) | u16::from(c);
        }
        self.times[(self.frames % HISTORY_FRAMES as u64) as usize] = now_ms;
        self.frames += 1;
        if matches!(self.state, State::Active { .. }) {
            return None;
        }
        if now_ms < self.not_before_ms.max(self.suppressed_until_ms) {
            self.state = State::Idle;
            return None;
        }
        let result = self.entry_rect(now_ms, bytes_per_block);
        if self.trace.is_some() {
            let chave = result.as_ref().err().map(Motivo::chave);
            if chave != self.last_motivo {
                if let Err(m) = &result {
                    self.log(now_ms, || format!("recusa: {m}"));
                }
                self.last_motivo = chave;
            }
        }
        match (result, self.state) {
            (Err(_), State::Candidate { rect, .. }) => {
                self.log(now_ms, || format!("Candidate({}) -> Idle", fmt_rect(rect)));
                self.state = State::Idle;
                None
            }
            (Err(_), _) => {
                self.state = State::Idle;
                None
            }
            (Ok(r), State::Candidate { rect, since_ms }) if rect == r => {
                if now_ms.saturating_sub(since_ms) >= ENTRY_HOLD_MS {
                    self.log(now_ms, || format!("Start({}) depois de {} ms como candidato", fmt_rect(r), now_ms.saturating_sub(since_ms)));
                    self.state = State::Active { rect: r, last_good_ms: now_ms, last_damage_ms: now_ms };
                    Some(Action::Start(r))
                } else {
                    None
                }
            }
            (Ok(r), State::Candidate { rect, .. }) => {
                self.log(now_ms, || format!("Candidate({}) -> Candidate({}): retângulo diferente", fmt_rect(rect), fmt_rect(r)));
                self.state = State::Candidate { rect: r, since_ms: now_ms };
                None
            }
            (Ok(r), _) => {
                self.log(now_ms, || format!("Idle -> Candidate({})", fmt_rect(r)));
                self.state = State::Candidate { rect: r, since_ms: now_ms };
                None
            }
        }
    }

    /// O retângulo de vídeo que a condição de entrada aceita agora, se algum.
    fn entry_rect(&self, now_ms: u64, bytes_per_block: f64) -> Result<BlockRect, Motivo> {
        if self.frames < HISTORY_FRAMES as u64 {
            return Err(Motivo::PoucosQuadros(self.frames));
        }
        if bytes_per_block < MIN_BYTES_PER_BLOCK {
            return Err(Motivo::CustoBaixo(bytes_per_block));
        }
        // Depois de gravar o quadro n-1, a posição `n % 16` guarda o mais antigo dos 16 últimos.
        let oldest = self.times[(self.frames % HISTORY_FRAMES as u64) as usize];
        if now_ms.saturating_sub(oldest) > HOT_WINDOW_MS {
            return Err(Motivo::ForaDaJanela(now_ms.saturating_sub(oldest)));
        }
        let recent_mask = (1u16 << HOT_RECENT_FRAMES) - 1;
        let hot = |bx: u32, by: u32| {
            let h = self.history[by as usize * self.cols + bx as usize];
            h.count_ones() >= HOT_MIN_CHANGES && h & recent_mask != 0
        };
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for by in 0..self.elig_rows {
            for bx in 0..self.elig_cols {
                if hot(bx, by) {
                    x0 = x0.min(bx);
                    y0 = y0.min(by);
                    x1 = x1.max(bx + 1);
                    y1 = y1.max(by + 1);
                }
            }
        }
        if x0 == u32::MAX {
            return Err(Motivo::NenhumBlocoQuente);
        }
        let mut rect = BlockRect { bx: x0, by: y0, bw: x1 - x0, bh: y1 - y0 };
        // Tamanho máximo: tira blocos da direita e de baixo até caber.
        while rect.to_rect(self.width, self.height).w > MAX_REGION_W_PX {
            rect.bw -= 1;
        }
        while rect.to_rect(self.width, self.height).h > MAX_REGION_H_PX {
            rect.bh -= 1;
        }
        let px = rect.to_rect(self.width, self.height);
        if px.w < MIN_REGION_PX || px.h < MIN_REGION_PX {
            return Err(Motivo::Pequeno(px.w, px.h));
        }
        let hot_in_rect = (rect.by..rect.by + rect.bh).flat_map(|by| (rect.bx..rect.bx + rect.bw).map(move |bx| (bx, by))).filter(|&(bx, by)| hot(bx, by)).count() as u32;
        if hot_in_rect * 100 < MIN_HOT_PERCENT * rect.blocks() {
            return Err(Motivo::PoucosQuentes(hot_in_rect, rect.blocks()));
        }
        Ok(rect)
    }

    /// Dano que caiu na região de vídeo em `now_ms`: `covered` blocos da região tocados, de `total`.
    /// Sem efeito fora do modo vídeo.
    pub fn on_damage(&mut self, now_ms: u64, covered: u32, total: u32) {
        if let State::Active { last_good_ms, last_damage_ms, .. } = &mut self.state {
            if covered > 0 {
                *last_damage_ms = now_ms;
            }
            if covered * 100 >= total * EXIT_MIN_COVERAGE_PERCENT {
                *last_good_ms = now_ms;
            }
        }
    }

    /// Verifica a saída por tempo. Chamar a cada passada do laço da sessão (não depende de quadros).
    pub fn poll(&mut self, now_ms: u64) -> Option<Action> {
        let State::Active { last_good_ms, last_damage_ms, .. } = self.state else { return None };
        if now_ms.saturating_sub(last_good_ms) < EXIT_QUIET_MS {
            return None;
        }
        let reason = if now_ms.saturating_sub(last_damage_ms) >= EXIT_QUIET_MS { StopReason::NoDamage } else { StopReason::LowCoverage };
        self.state = State::Idle;
        self.not_before_ms = now_ms + REENTRY_COOLDOWN_MS;
        self.clear_history();
        Some(Action::Stop(reason))
    }

    /// O codificador falhou (ou não abriu): sai do vídeo e não tenta de novo por
    /// [`ENCODER_FAILURE_COOLDOWN_MS`].
    pub fn encoder_failed(&mut self, now_ms: u64) {
        self.state = State::Idle;
        self.suppressed_until_ms = now_ms + ENCODER_FAILURE_COOLDOWN_MS;
        self.clear_history();
    }

    /// Nenhuma janela deve entrar em vídeo antes de `until_ms` (erro do codificador em outra janela).
    pub fn suppress_until(&mut self, until_ms: u64) {
        self.suppressed_until_ms = self.suppressed_until_ms.max(until_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Janela de 1280×720: 20 colunas, 12 linhas de blocos (a última é parcial).
    const W: u32 = 1280;
    const H: u32 = 720;
    const COLS: usize = 20;
    const ROWS: usize = 12;
    const PASSO: u64 = 33;
    const CARO: f64 = 4000.0;

    /// Blocos (colunas bx0..bx1, linhas by0..by1) marcados como mudados.
    fn regiao(bx0: usize, by0: usize, bx1: usize, by1: usize) -> Vec<bool> {
        let mut v = vec![false; COLS * ROWS];
        for by in by0..by1 {
            for bx in bx0..bx1 {
                v[by * COLS + bx] = true;
            }
        }
        v
    }

    fn nada() -> Vec<bool> {
        vec![false; COLS * ROWS]
    }

    /// Alimenta `n` quadros iguais a partir de `t0`, a cada `passo` ms. Devolve a primeira ação e o instante dela.
    fn alimenta(d: &mut Detector, t0: u64, passo: u64, n: usize, changed: &[bool], bpb: f64) -> Option<(u64, Action)> {
        for i in 0..n {
            let t = t0 + passo * i as u64;
            if let Some(a) = d.on_frame(t, changed, bpb) {
                return Some((t, a));
            }
        }
        None
    }

    #[test]
    fn entra_em_video_com_regiao_animada() {
        let mut d = Detector::new(W, H);
        assert_eq!(d.cols(), COLS);
        let (t, a) = alimenta(&mut d, 0, PASSO, 200, &regiao(1, 1, 11, 7), CARO).expect("deveria entrar");
        assert_eq!(a, Action::Start(BlockRect { bx: 1, by: 1, bw: 10, bh: 6 }));
        // 16 quadros (o 16º em 495 ms) e depois 500 ms seguidos: entre 1 s e 2 s.
        assert!((995..2000).contains(&t), "entrou em {t} ms");
        assert!(d.is_active());
        assert_eq!(d.to_pixels(d.region().unwrap()), ps_codec::Rect::new(64, 64, 640, 384));
        // Depois do Start só atualiza o histórico: nenhuma outra ação.
        assert_eq!(d.on_frame(t + PASSO, &regiao(1, 1, 11, 7), CARO), None);
    }

    #[test]
    fn rastro_desligado_nao_registra_nada_e_ligado_registra_so_transicoes() {
        let mut mudo = Detector::new(W, H);
        alimenta(&mut mudo, 0, PASSO, 200, &regiao(1, 1, 11, 7), CARO).expect("entra");
        assert!(mudo.take_trace().is_empty(), "sem enable_trace não se registra nada");

        let mut d = Detector::new(W, H);
        d.enable_trace();
        let (t, _) = alimenta(&mut d, 0, PASSO, 200, &regiao(1, 1, 11, 7), CARO).expect("entra");
        let linhas = d.take_trace();
        let texto = linhas.join("
");
        assert!(linhas.iter().any(|l| l.contains("recusa: menos de 16 quadros")), "{texto}");
        assert!(linhas.iter().any(|l| l.contains("Idle -> Candidate(10x6+1+1 (blocos))")), "{texto}");
        assert!(linhas.last().unwrap().contains(&format!("t={t}ms Start(10x6+1+1 (blocos))")), "{texto}");
        // Uma linha por transição, não por quadro: 16 quadros sem histórico viram uma linha só.
        assert_eq!(linhas.iter().filter(|l| l.contains("menos de 16 quadros")).count(), 1, "{texto}");
        assert!(d.take_trace().is_empty(), "recolher esvazia");
    }

    #[test]
    fn rastro_diz_o_motivo_de_cada_recusa_e_o_retangulo_que_muda() {
        let mut d = Detector::new(W, H);
        d.enable_trace();
        // Custo baixo, depois sem blocos quentes (nada muda), depois região pequena.
        alimenta(&mut d, 0, PASSO, 20, &regiao(1, 1, 11, 7), 900.0);
        alimenta(&mut d, 20 * PASSO, PASSO, 40, &vec![false; COLS * ROWS], CARO);
        let l = d.take_trace().join("
");
        assert!(l.contains("custo por bloco 900 abaixo de 1024"), "{l}");
        assert!(l.contains("nenhum bloco quente"), "{l}");
        let mut p = Detector::new(W, H);
        p.enable_trace();
        alimenta(&mut p, 0, PASSO, 40, &regiao(1, 1, 3, 3), CARO);
        assert!(p.take_trace().join("
").contains("retângulo pequeno 128x128"));
        // Retângulo que muda: as duas linhas aparecem.
        let mut m = Detector::new(W, H);
        m.enable_trace();
        for i in 0..40u64 {
            let mut c = regiao(1, 1, 11, 7);
            if i >= 8 {
                for by in 1..7 {
                    c[by * COLS + 11] = true;
                }
            }
            m.on_frame(i * PASSO, &c, CARO);
        }
        let l = m.take_trace().join("
");
        assert!(l.contains("Candidate(10x6+1+1 (blocos)) -> Candidate(11x6+1+1 (blocos)): retângulo diferente"), "{l}");
    }

    #[test]
    fn nao_entra_se_o_codec_de_blocos_e_barato() {
        let mut d = Detector::new(W, H);
        assert_eq!(alimenta(&mut d, 0, PASSO, 300, &regiao(1, 1, 11, 7), 900.0), None);
        // Basta o custo subir para entrar.
        assert!(alimenta(&mut d, 300 * PASSO, PASSO, 100, &regiao(1, 1, 11, 7), 1024.0).is_some());
    }

    #[test]
    fn nao_entra_com_cursor_piscando() {
        let mut d = Detector::new(W, H);
        // Um bloco muda a cada 15 quadros (cerca de 2 Hz a 30 quadros por segundo), o resto fica parado.
        for i in 0..600u64 {
            let c = if i % 15 == 0 { regiao(3, 4, 4, 5) } else { nada() };
            assert_eq!(d.on_frame(i * PASSO, &c, CARO), None, "quadro {i}");
        }
        // Mesmo mudando a cada quadro, um bloco só é pequeno demais.
        for i in 600..900u64 {
            assert_eq!(d.on_frame(i * PASSO, &regiao(3, 4, 4, 5), CARO), None);
        }
    }

    #[test]
    fn nao_entra_com_rolagem_de_texto_breve() {
        let mut d = Detector::new(W, H);
        // Rolagem de 20 quadros a 60 por segundo (cerca de 0,3 s) numa área grande...
        assert_eq!(alimenta(&mut d, 0, 16, 20, &regiao(2, 1, 16, 9), CARO), None);
        // ... e depois só um cursor piscando, com quadros dentro dos 2 s do histórico.
        for i in 0..8u64 {
            let c = if i % 2 == 0 { regiao(3, 4, 4, 5) } else { nada() };
            assert_eq!(d.on_frame(400 + i * 200, &c, CARO), None, "quadro de cursor {i}");
        }
        // A rolagem em si, mais curta que a espera de 500 ms, também não entra.
        let mut d = Detector::new(W, H);
        assert_eq!(alimenta(&mut d, 0, 16, 40, &regiao(2, 1, 16, 9), CARO), None, "40 quadros a 60 Hz (0,62 s): a espera de 500 ms só acaba no quadro 47");
    }

    #[test]
    fn rolagem_longa_entra() {
        // Contraprova do teste anterior: a mesma área, mantida além de 16 quadros mais 500 ms, é vídeo.
        let mut d = Detector::new(W, H);
        assert!(alimenta(&mut d, 0, 16, 120, &regiao(2, 1, 16, 9), CARO).is_some());
    }

    #[test]
    fn nao_entra_com_regiao_pequena() {
        let mut d = Detector::new(W, H);
        // 3×4 e 4×3 blocos: abaixo de 256×256.
        assert_eq!(alimenta(&mut d, 0, PASSO, 300, &regiao(2, 2, 5, 6), CARO), None);
        assert_eq!(alimenta(&mut d, 20_000, PASSO, 300, &regiao(2, 2, 6, 5), CARO), None);
        // 4×4 é o mínimo.
        let mut d = Detector::new(W, H);
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &regiao(2, 2, 6, 6), CARO).expect("4x4 entra");
        assert_eq!(a, Action::Start(BlockRect { bx: 2, by: 2, bw: 4, bh: 4 }));
    }

    #[test]
    fn exige_oitenta_por_cento_de_blocos_quentes() {
        // Retângulo envolvente de 10×6 = 60 blocos, com os 4 cantos opostos quentes: o miolo está parado.
        let esparso = |frio: usize| {
            let mut v = regiao(1, 1, 11, 7);
            // Esfria `frio` blocos do interior (sem tocar nos que definem o retângulo).
            for k in 0..frio {
                v[(2 + k / 8) * COLS + 2 + k % 8] = false;
            }
            v
        };
        let mut d = Detector::new(W, H);
        assert_eq!(alimenta(&mut d, 0, PASSO, 300, &esparso(13), CARO), None, "47 de 60 = 78%");
        let mut d = Detector::new(W, H);
        assert!(alimenta(&mut d, 0, PASSO, 300, &esparso(12), CARO).is_some(), "48 de 60 = 80%");
    }

    #[test]
    fn retangulo_que_muda_reinicia_a_espera() {
        let mut d = Detector::new(W, H);
        // A região A (10 colunas) muda a cada quadro. A coluna 11 só começa a mudar no quadro 8 e
        // vira quente no quadro 19 (t = 627 ms): o retângulo passa a ter 11 colunas antes de A
        // completar os 500 ms de espera (que acabariam em 995 ms).
        let mut inicio = None;
        for i in 0..200u64 {
            let mut c = regiao(1, 1, 11, 7);
            if i >= 8 {
                for by in 1..7 {
                    c[by * COLS + 11] = true;
                }
            }
            if let Some(a) = d.on_frame(i * PASSO, &c, CARO) {
                inicio = Some((i * PASSO, a));
                break;
            }
        }
        let (t, a) = inicio.expect("entra com o retângulo novo");
        assert_eq!(a, Action::Start(BlockRect { bx: 1, by: 1, bw: 11, bh: 6 }));
        assert!(t >= 19 * PASSO + ENTRY_HOLD_MS, "entrou em {t} ms: a espera recomeça quando o retângulo muda");
    }

    #[test]
    fn a_regiao_chega_ate_a_borda_da_janela() {
        // 1000×700 (pares): 16 colunas e 11 linhas de blocos, as últimas parciais (40 e 60 px).
        let mut d = Detector::new(1000, 700);
        assert_eq!((d.cols(), d.elig_cols, d.elig_rows), (16, 16, 11));
        let todos = vec![true; 16 * 11];
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &todos, CARO).expect("entra");
        assert_eq!(a, Action::Start(BlockRect { bx: 0, by: 0, bw: 16, bh: 11 }));
        assert_eq!(d.to_pixels(BlockRect { bx: 0, by: 0, bw: 16, bh: 11 }), ps_codec::Rect::new(0, 0, 1000, 700));
        // 1280×720 todo animado: a região é a janela inteira (720 não é múltiplo de 64).
        let mut d = Detector::new(1280, 720);
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &vec![true; COLS * ROWS], CARO).expect("entra");
        let Action::Start(r) = a else { unreachable!() };
        assert_eq!(d.to_pixels(r), ps_codec::Rect::new(0, 0, 1280, 720));
    }

    #[test]
    fn dimensao_impar_deixa_a_ultima_coluna_ou_linha_de_fora() {
        // 1001×701: a coluna 15 (1 px de largura) e a linha 10 (61 px de altura é ímpar: 701 - 640) ficam fora.
        let mut d = Detector::new(1001, 701);
        assert_eq!((d.cols(), d.elig_cols, d.elig_rows), (16, 15, 10));
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &[true; 16 * 11], CARO).expect("entra");
        let Action::Start(r) = a else { unreachable!() };
        assert_eq!(r, BlockRect { bx: 0, by: 0, bw: 15, bh: 10 });
        let px = d.to_pixels(r);
        assert_eq!(px, ps_codec::Rect::new(0, 0, 960, 640));
        assert!(px.w.is_multiple_of(2) && px.h.is_multiple_of(2));
        // Só a largura é ímpar: a altura chega até a borda.
        let mut d = Detector::new(1001, 700);
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &[true; 16 * 11], CARO).expect("entra");
        let Action::Start(r) = a else { unreachable!() };
        assert_eq!(d.to_pixels(r), ps_codec::Rect::new(0, 0, 960, 700));
    }

    #[test]
    fn regiao_grande_e_limitada_a_1920_por_1080() {
        let mut d = Detector::new(2560, 1280);
        let todos = vec![true; 40 * 20];
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &todos, CARO).expect("entra");
        let Action::Start(r) = a else { unreachable!() };
        // 17 linhas de blocos dariam 1088 px: cai para 16 (1024 px).
        assert_eq!(d.to_pixels(r), ps_codec::Rect::new(0, 0, 1920, 1024));
        // Uma janela de 1920×1080 inteira cabe no limite.
        let mut d = Detector::new(1920, 1080);
        let (_, a) = alimenta(&mut d, 0, PASSO, 300, &vec![true; 30 * 17], CARO).expect("entra");
        let Action::Start(r) = a else { unreachable!() };
        assert_eq!(d.to_pixels(r), ps_codec::Rect::new(0, 0, 1920, 1080));
    }

    #[test]
    fn declinar_o_start_devolve_o_detector_a_idle() {
        let mut d = Detector::new(W, H);
        let (t, _) = alimenta(&mut d, 0, PASSO, 200, &regiao(1, 1, 11, 7), CARO).unwrap();
        assert!(d.is_active());
        d.decline();
        assert!(!d.is_active());
        // Pode voltar a entrar, depois de nova espera.
        let (t2, _) = alimenta(&mut d, t + PASSO, PASSO, 200, &regiao(1, 1, 11, 7), CARO).expect("volta");
        assert!(t2 >= t + ENTRY_HOLD_MS);
    }

    fn em_video() -> (Detector, u64) {
        let mut d = Detector::new(W, H);
        let (t, _) = alimenta(&mut d, 0, PASSO, 200, &regiao(1, 1, 11, 7), CARO).unwrap();
        d.on_damage(t, 60, 60);
        (d, t)
    }

    #[test]
    fn sai_depois_de_um_segundo_sem_dano() {
        let (mut d, t) = em_video();
        // Dano de 30 em 30 ms até t + 300 ms; depois silêncio.
        for k in 1..=10 {
            d.on_damage(t + k * 30, 60, 60);
            assert_eq!(d.poll(t + k * 30), None);
        }
        let ultimo = t + 300;
        assert_eq!(d.poll(ultimo + 999), None, "999 ms sem dano ainda é vídeo");
        assert_eq!(d.poll(ultimo + 1000), Some(Action::Stop(StopReason::NoDamage)));
        assert!(!d.is_active());
        assert_eq!(d.poll(ultimo + 5000), None, "já saiu");
    }

    #[test]
    fn sai_quando_o_dano_cobre_menos_da_metade_por_um_segundo() {
        let (mut d, t) = em_video();
        // Dano em 20% da região a cada 33 ms: há dano, mas nunca metade.
        for k in 1..=30 {
            d.on_damage(t + k * 33, 12, 60);
            assert_eq!(d.poll(t + k * 33), None, "k = {k}");
        }
        d.on_damage(t + 1023, 12, 60);
        assert_eq!(d.poll(t + 1023), Some(Action::Stop(StopReason::LowCoverage)));
        assert!(!d.is_active());
    }

    #[test]
    fn um_quadro_com_metade_de_cobertura_zera_a_espera_da_saida() {
        let (mut d, t) = em_video();
        for k in 1..=25 {
            d.on_damage(t + k * 33, 12, 60);
        }
        d.on_damage(t + 900, 30, 60); // metade da região: movimento de verdade
        d.on_damage(t + 1500, 12, 60);
        assert_eq!(d.poll(t + 1500), None, "só 600 ms desde o último dano que cobriu metade");
        assert_eq!(d.poll(t + 1900), Some(Action::Stop(StopReason::LowCoverage)));
    }

    #[test]
    fn dano_que_cobre_metade_mantem_o_video() {
        let (mut d, t) = em_video();
        for k in 1..=100 {
            // Alterna 29 e 30 blocos de 60: 30 (metade) conta como movimento.
            d.on_damage(t + k * 33, if k % 2 == 0 { 30 } else { 29 }, 60);
            assert_eq!(d.poll(t + k * 33), None, "k = {k}");
        }
    }

    #[test]
    fn respeita_os_intervalos_depois_de_sair() {
        let (mut d, t) = em_video();
        let saida = t + EXIT_QUIET_MS;
        assert!(matches!(d.poll(saida), Some(Action::Stop(_))));
        // Depois de sair o histórico recomeça: com quadros a cada 33 ms a volta só acontece
        // 16 quadros + 500 ms depois, nunca antes de 500 ms.
        let (t2, a) = alimenta(&mut d, saida, PASSO, 300, &regiao(1, 1, 11, 7), CARO).expect("volta");
        assert!(matches!(a, Action::Start(_)));
        assert!(t2 >= saida + REENTRY_COOLDOWN_MS, "voltou {} ms depois", t2 - saida);
        // Quadros muito rápidos (1 ms) enchem o histórico em 16 ms, mas ainda assim a volta espera 500 ms.
        let (mut d, t) = em_video();
        let saida = t + EXIT_QUIET_MS;
        assert!(matches!(d.poll(saida), Some(Action::Stop(_))));
        let (t3, _) = alimenta(&mut d, saida, 1, 3000, &regiao(1, 1, 11, 7), CARO).expect("volta");
        assert!(t3 >= saida + REENTRY_COOLDOWN_MS, "voltou {} ms depois", t3 - saida);
    }

    #[test]
    fn erro_do_codificador_suspende_por_dez_segundos() {
        let (mut d, t) = em_video();
        d.encoder_failed(t);
        assert!(!d.is_active());
        assert_eq!(alimenta(&mut d, t, PASSO, 250, &regiao(1, 1, 11, 7), CARO), None, "8 s de quadros quentes: ainda suspenso");
        let (t2, _) = alimenta(&mut d, t + ENCODER_FAILURE_COOLDOWN_MS - 100, PASSO, 300, &regiao(1, 1, 11, 7), CARO).expect("volta depois dos 10 s");
        assert!(t2 >= t + ENCODER_FAILURE_COOLDOWN_MS, "voltou em {t2}");
        // `suppress_until` vale para janelas que não falharam e sobrevive ao `reset`.
        let mut d = Detector::new(W, H);
        d.suppress_until(5000);
        d.reset(W, H);
        assert_eq!(alimenta(&mut d, 0, PASSO, 150, &regiao(1, 1, 11, 7), CARO), None);
    }

    #[test]
    fn reset_por_redimensionamento_esquece_tudo() {
        let (mut d, _) = em_video();
        d.reset(800, 600);
        assert!(!d.is_active());
        assert_eq!(d.cols(), 13);
        // Quadro de tamanho antigo é ignorado em vez de dar pânico.
        assert_eq!(d.on_frame(0, &regiao(1, 1, 11, 7), CARO), None);
    }
}
