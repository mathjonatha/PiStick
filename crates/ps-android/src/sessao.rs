//! A sessão do cliente Android com o aparelho: reaproveita a rede, o canal cifrado, a descoberta e o registro do
//! `ps-client` e liga tudo à fila de eventos da casca, à tela e à entrada (docs/26, decisões 5 a 7, 10 a 12 e 17).
//!
//! Não tem nenhuma chamada do Android: a superfície é um [`Tela`] (no aparelho, `surface.rs`; nos testes, uma falsa) e
//! o que o núcleo diz à casca sai por [`Fila`]. Por isso tudo roda nos testes do computador, inclusive contra um `psd`
//! de verdade no laço local (`tests/sessao.rs`).
//!
//! Duas camadas:
//! - [`Sessao`]: uma conexão já aberta. Recebe os eventos da rede (é o [`UiSink`] de `ps_client::net`), mantém as
//!   janelas, a vista e a entrada, e desenha;
//! - [`Controle`]: o que a casca controla: busca, conexão (com a chave do cliente), tela, entrada. Uma conexão por vez.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ps_client::audio::{AudioEngine, AudioSink};
use ps_client::clipboard::{ClipboardSync, HostClipboard};
use ps_client::compose::{compor, Camada};
use ps_client::discovery;
use ps_client::net::{self, SharedWindow, UiSink, UserEvent};
use ps_client::registry::Registry;
use ps_client::secure::{self, Connected, SecurityOptions};
use ps_client::stats::Stats;
use ps_client::CLIENT_CAPS;
use ps_proto::{read_hello, write_message, Message, CAP_ATOMIC, CAP_AUDIO, CAP_H264};
use ps_secure::Keypair;
use ps_transport::{with_deadline, Closer, Split, TcpTransport, Transport};

use crate::entrada::{Cena as CenaEntrada, Mapeador};
use crate::eventos::{fase, Evento, Fila, Segredo};
use crate::gestos::{Gestos, Saida, Toque};
use crate::janelas::{Janelas, Limites, Mudanca};
use crate::layout::{desenhar_rgba, fator_de_centesimos, limitar_fator, pedido_de_tamanho, Tela as TelaInfo, Vista, FATOR_PADRAO};
use crate::registro;
use crate::teclado::{Mod, Passo, Teclado};
use crate::tela::Tela;

/// Porta padrão do protocolo, quando o endereço digitado não traz uma.
pub const PORTA_PADRAO: u16 = 7700;
/// Limite para abrir a conexão TCP.
const TEMPO_CONEXAO: Duration = Duration::from_secs(5);
/// Limite para o `HELLO` depois do canal cifrado aberto.
const TEMPO_HELLO: Duration = Duration::from_secs(10);
/// Quanto o núcleo espera a casca gravar a chave nova (o Keystore responde em milissegundos).
const TEMPO_CHAVE: Duration = Duration::from_secs(20);
/// Pausa entre apertar e soltar o botão de um toque (o `psc` usa 40 ms nos roteiros).
const PAUSA_BOTAO_MS: u32 = 30;
/// A cada quantos quadros desenhados o tempo médio da cópia vai para o `logcat`.
const QUADROS_POR_MEDIDA: u64 = 120;

type SuperficieComum = Arc<Mutex<Option<Box<dyn Tela>>>>;

/// Como abrir a saída de som do sistema para um motor de áudio (no Android, o AAudio). Sem fábrica, ou se ela falha,
/// o cliente não anuncia `CAP_AUDIO`.
pub type FabricaDeSaida = fn(Arc<AudioEngine>) -> Result<Box<dyn AudioSink>, String>;

fn travar<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A área de transferência do celular, vista pelo `ClipboardSync`: o texto vem da casca (ao voltar ao foco) e o que o
/// Pi manda sai como evento para a casca gravar no `ClipboardManager`.
struct AreaDoCelular {
    texto: Mutex<Option<String>>,
    fila: Arc<Fila>,
}

impl HostClipboard for AreaDoCelular {
    fn get_text(&self) -> Option<String> {
        travar(&self.texto).clone()
    }

    fn set_text(&self, text: &str) -> bool {
        *travar(&self.texto) = Some(text.to_string());
        self.fila.empurrar(Evento::AreaDeTransferencia(text.to_string()));
        true
    }
}

/// Estado mutável de uma sessão, atrás de uma só trava.
struct Cena {
    janelas: Janelas,
    shared: HashMap<u32, Arc<SharedWindow>>,
    vista: Vista,
    tela: TelaInfo,
    fator: f32,
    mapeador: Mapeador,
    gestos: Gestos,
    teclado: Teclado,
    /// Último `RESIZE_REQ` enviado por janela (para não repetir pedidos iguais).
    pedidos: HashMap<u32, (u32, u32)>,
    /// Composto do último desenho: tamanho e origem (para acertar os toques).
    composto: (u32, u32),
    origem: (i32, i32),
    rgba: Vec<u8>,
    app_em_foco: bool,
    /// Medida do tempo de desenho (compor, converter, publicar): soma em microssegundos e quadros.
    tempo_us: u64,
    quadros: u64,
}

/// Uma conexão aberta.
pub struct Sessao {
    fila: Arc<Fila>,
    out: Sender<Message>,
    entrada_tx: Sender<Vec<Passo>>,
    superficie: SuperficieComum,
    cena: Mutex<Cena>,
    area: Arc<AreaDoCelular>,
    clip: Arc<ClipboardSync>,
    closer: Option<Closer>,
    parado: AtomicBool,
    audio: Option<Arc<AudioEngine>>,
}

impl Sessao {
    /// Monta a sessão sobre o canal de saída `out` (as mensagens que o núcleo manda ao aparelho). Devolve também a
    /// ponta que a thread de entrada consome ([`Sessao::executar_entrada`]); os testes a esvaziam à mão.
    pub fn nova(fila: Arc<Fila>, out: Sender<Message>, superficie: SuperficieComum, tela: TelaInfo, closer: Option<Closer>, audio: Option<Arc<AudioEngine>>) -> (Arc<Sessao>, Receiver<Vec<Passo>>) {
        Self::nova_com_fator(fila, out, superficie, tela, FATOR_PADRAO, closer, audio)
    }

    /// Como [`Sessao::nova`], com o fator de escala já escolhido (limitado ao passo e aos limites).
    pub fn nova_com_fator(fila: Arc<Fila>, out: Sender<Message>, superficie: SuperficieComum, tela: TelaInfo, fator: f32, closer: Option<Closer>, audio: Option<Arc<AudioEngine>>) -> (Arc<Sessao>, Receiver<Vec<Passo>>) {
        let fator = limitar_fator(fator);
        let (entrada_tx, entrada_rx) = mpsc::channel();
        let area = Arc::new(AreaDoCelular { texto: Mutex::new(None), fila: fila.clone() });
        let clip = ClipboardSync::new(area.clone(), out.clone());
        let cena = Cena {
            janelas: Janelas::new(),
            shared: HashMap::new(),
            vista: Vista::nova(&tela, fator),
            tela,
            fator,
            mapeador: Mapeador::new(),
            gestos: Gestos::new(tela.dpi),
            teclado: Teclado::new(),
            pedidos: HashMap::new(),
            composto: (0, 0),
            origem: (0, 0),
            rgba: Vec::new(),
            app_em_foco: true,
            tempo_us: 0,
            quadros: 0,
        };
        let s = Arc::new(Sessao { fila, out, entrada_tx, superficie, cena: Mutex::new(cena), area, clip, closer, parado: AtomicBool::new(false), audio });
        (s, entrada_rx)
    }

    pub fn clipboard(&self) -> Arc<ClipboardSync> {
        self.clip.clone()
    }

    fn mandar(&self, m: Message) {
        let _ = self.out.send(m);
    }

    /// Fecha a conexão e para de aceitar eventos.
    pub fn parar(&self) {
        self.parado.store(true, Ordering::SeqCst);
        if let Some(c) = &self.closer {
            c.close();
        }
        // Para a saída de som e solta o dispositivo.
        if let Some(a) = &self.audio {
            a.shutdown();
        }
    }

    pub fn parada(&self) -> bool {
        self.parado.load(Ordering::SeqCst)
    }

    // ---- janelas, foco, tamanho -------------------------------------------------------------------------------

    fn publicar_janelas(&self, c: &Cena) {
        self.fila.empurrar(Evento::Janelas { foco: c.janelas.foco().unwrap_or(0), abas: c.janelas.abas() });
    }

    /// Pede ao servidor o tamanho que cabe na tela para `win`, se ainda não pediu esse.
    fn pedir_tamanho(&self, c: &mut Cena, win: u32) {
        if c.tela.largura == 0 || c.tela.altura == 0 {
            return;
        }
        let lim = c.janelas.limites(win);
        let tam = pedido_de_tamanho(&c.tela, c.fator, &lim);
        if c.pedidos.get(&win) != Some(&tam) {
            c.pedidos.insert(win, tam);
            self.mandar(Message::ResizeReq { win, width: tam.0, height: tam.1 });
        }
    }

    /// Aplica o que uma mudança de janelas fez: lista de abas, foco (avisa o servidor), tamanho e desenho.
    fn aplicar_mudanca(&self, c: &mut Cena, m: Mudanca) {
        if m.abas || m.foco_mudou() {
            self.publicar_janelas(c);
        }
        if m.foco_mudou() {
            c.mapeador.reiniciar();
            c.vista.voltar_ao_ajuste(c.composto);
            if c.app_em_foco {
                if let Some(antes) = m.foco_antes {
                    self.mandar(Message::Focus { win: antes, focused: false });
                }
                if let Some(depois) = m.foco_depois {
                    self.mandar(Message::Focus { win: depois, focused: true });
                }
            }
            if let Some(depois) = m.foco_depois {
                self.pedir_tamanho(c, depois);
            }
            self.desenhar(c);
        }
    }

    /// A janela está na tela agora (a em foco ou um popup dela)?
    fn visivel(c: &Cena, win: u32) -> bool {
        match c.janelas.foco() {
            Some(f) => f == win || c.janelas.popups_de(f).iter().any(|p| p.id == win),
            None => false,
        }
    }

    fn confirmar(&self, win: u32, acks: Vec<u32>) {
        for seq in acks {
            self.mandar(Message::FrameAck { win, seq });
        }
    }

    fn tirar_acks(shared: &SharedWindow) -> Vec<u32> {
        std::mem::take(&mut shared.inner.lock().unwrap_or_else(|e| e.into_inner()).acks)
    }

    /// Compõe a janela em foco com os popups, aplica a vista, copia para a tela e confirma os quadros.
    fn desenhar(&self, c: &mut Cena) {
        let Some(raiz) = c.janelas.foco() else { return };
        let t0 = Instant::now();
        let popups = c.janelas.popups_de(raiz);
        let mut camadas = Vec::with_capacity(1 + popups.len());
        let mut acks: Vec<(u32, Vec<u32>)> = Vec::new();
        let mut ler = |id: u32, parent: u32, x: i32, y: i32, camadas: &mut Vec<Camada>| {
            if let Some(sh) = c.shared.get(&id) {
                let mut g = sh.inner.lock().unwrap_or_else(|e| e.into_inner());
                camadas.push(Camada { id, parent, x, y, w: g.decoder.width(), h: g.decoder.height(), pixels: g.display_pixels().to_vec() });
                let a = std::mem::take(&mut g.acks);
                if !a.is_empty() {
                    acks.push((id, a));
                }
            }
        };
        ler(raiz, 0, 0, 0, &mut camadas);
        // Posição absoluta, com a raiz como mãe (a composição soma a posição da mãe, que é (0, 0)).
        for p in &popups {
            ler(p.id, raiz, p.x, p.y, &mut camadas);
        }
        let composto = compor(&camadas);
        let mut publicou = Ok(());
        if let Some(comp) = composto {
            c.composto = (comp.largura, comp.altura);
            c.origem = (comp.origem_x, comp.origem_y);
            c.vista.ajustar(c.composto);
            let (w, h) = desenhar_rgba(&comp, &c.vista, &mut c.rgba);
            if let Some(tela) = travar(&self.superficie).as_mut() {
                publicou = tela.publicar(w, h, &c.rgba);
            }
        }
        if let Err(e) = publicou {
            registro::erro(&format!("não foi possível desenhar na superfície: {e}"));
        }
        // Os quadros apresentados são confirmados mesmo se a tela falhou (senão o fluxo do servidor trava).
        for (id, a) in acks {
            self.confirmar(id, a);
        }
        c.tempo_us += t0.elapsed().as_micros() as u64;
        c.quadros += 1;
        if c.quadros.is_multiple_of(QUADROS_POR_MEDIDA) {
            registro::info(&format!("desenho (compor, converter e copiar para a Surface): média de {} µs em {} quadros", c.tempo_us / c.quadros, c.quadros));
        }
    }

    // ---- eventos da rede --------------------------------------------------------------------------------------

    fn tratar(&self, ev: UserEvent) {
        let mut c = travar(&self.cena);
        match ev {
            UserEvent::WinCreate { win, width, height, title, shared } => {
                c.shared.insert(win, shared);
                let m = c.janelas.criar(win, width, height, &title);
                self.aplicar_mudanca(&mut c, m);
            }
            UserEvent::PopupCreate { win, parent, x, y, width, height, shared, .. } => {
                if c.janelas.criar_popup(win, parent, x, y, width, height) {
                    c.shared.insert(win, shared);
                } else {
                    // Mãe desconhecida: confirma o que vier e segue.
                    let a = Self::tirar_acks(&shared);
                    self.confirmar(win, a);
                }
            }
            UserEvent::PopupMove { win, x, y, width, height } => {
                c.janelas.mover_popup(win, x, y, width, height);
                if Self::visivel(&c, win) {
                    self.desenhar(&mut c);
                }
            }
            UserEvent::WinParent { .. } | UserEvent::Cursor { .. } | UserEvent::Quit => {}
            UserEvent::WinLimits { win, min_w, min_h, max_w, max_h } => {
                c.janelas.definir_limites(win, Limites { min_w, min_h, max_w, max_h });
                if c.janelas.foco() == Some(win) {
                    self.pedir_tamanho(&mut c, win);
                }
            }
            UserEvent::WinResize { win, width, height } => {
                c.janelas.redimensionar(win, width, height);
                if Self::visivel(&c, win) {
                    self.desenhar(&mut c);
                }
            }
            UserEvent::WinTitle { win, title } => {
                if c.janelas.titulo(win, &title) {
                    self.publicar_janelas(&c);
                }
            }
            UserEvent::WinDestroy { win } => {
                let m = c.janelas.destruir(win);
                // Esquece os quadros das janelas que já não existem (a destruída e os popups dela).
                let vivas: Vec<u32> = c.janelas.abas().iter().map(|a| a.0).collect();
                let mantidos: Vec<u32> = vivas.iter().flat_map(|&r| c.janelas.popups_de(r).into_iter().map(|p| p.id).chain(std::iter::once(r))).collect();
                c.shared.retain(|id, _| mantidos.contains(id));
                c.pedidos.retain(|id, _| mantidos.contains(id));
                self.aplicar_mudanca(&mut c, m);
                if c.janelas.foco().is_some() && !m.foco_mudou() {
                    self.desenhar(&mut c);
                }
            }
            UserEvent::Frame { win, shared } => {
                if Self::visivel(&c, win) {
                    self.desenhar(&mut c);
                } else {
                    let a = Self::tirar_acks(&shared);
                    self.confirmar(win, a);
                }
            }
            UserEvent::Disconnected(motivo) => {
                self.fila.empurrar(Evento::Estado { fase: fase::DESCONECTADO, texto: motivo });
                self.parar();
            }
            // O aparelho se despediu (outro cliente assumiu, serviço parando): o texto vai à casca e a sessão acaba. O núcleo
            // nunca reconecta sozinho; com o motivo 1 a casca também não deve (dois aparelhos roubariam a sessão um do outro).
            UserEvent::Goodbye { reason, text } => {
                let texto = net::goodbye_text(reason, &text);
                self.fila.empurrar(Evento::Encerrada { motivo: reason, texto: texto.clone() });
                self.fila.empurrar(Evento::Estado { fase: fase::DESCONECTADO, texto });
                self.parar();
            }
            UserEvent::Status { flags, temp_dc } => self.fila.empurrar(Evento::EstadoDoAparelho { sinalizadores: flags, temperatura_dc: temp_dc }),
        }
    }

    // ---- tela -------------------------------------------------------------------------------------------------

    /// A superfície mudou de tamanho (rotação, teclado virtual, abas): refaz a vista e pede o tamanho novo.
    pub fn definir_tela(&self, tela: TelaInfo) {
        let mut c = travar(&self.cena);
        if c.tela == tela {
            return;
        }
        c.tela = tela;
        c.gestos = Gestos::new(tela.dpi);
        c.vista = Vista::nova(&tela, c.fator);
        if let Some(f) = c.janelas.foco() {
            self.pedir_tamanho(&mut c, f);
        }
        self.desenhar(&mut c);
    }

    /// O fator de escala mudou (botões A- e A+): refaz o pedido de tamanho de todas as janelas conhecidas e recria a vista
    /// (volta ao ajuste), sem derrubar a conexão.
    pub fn definir_fator(&self, fator: f32) {
        let fator = limitar_fator(fator);
        let mut c = travar(&self.cena);
        if c.fator == fator {
            return;
        }
        c.fator = fator;
        c.vista = Vista::nova(&c.tela, fator);
        c.pedidos.clear();
        let ids: Vec<u32> = c.janelas.abas().iter().map(|a| a.0).collect();
        for id in ids {
            self.pedir_tamanho(&mut c, id);
        }
        self.desenhar(&mut c);
    }

    /// Redesenha com o que há (a superfície foi criada ou trocada).
    pub fn redesenhar(&self) {
        let mut c = travar(&self.cena);
        self.desenhar(&mut c);
    }

    pub fn tela(&self) -> TelaInfo {
        travar(&self.cena).tela
    }

    // ---- entrada ----------------------------------------------------------------------------------------------

    /// Entrega passos à thread de entrada.
    fn enviar_passos(&self, p: Vec<Passo>) {
        if !p.is_empty() {
            let _ = self.entrada_tx.send(p);
        }
    }

    /// Roda a fila de entrada até a sessão acabar: manda as mensagens e faz as pausas. É a thread `psa-entrada`.
    pub fn executar_entrada(self: &Arc<Self>, rx: Receiver<Vec<Passo>>) {
        for lote in rx {
            for p in lote {
                match p {
                    Passo::Msg(m) => self.mandar(m),
                    Passo::PausaMs(ms) => std::thread::sleep(Duration::from_millis(u64::from(ms))),
                }
            }
        }
    }

    /// Os gestos do `MotionEvent`: traduz em mensagens (com uma pausa entre apertar e soltar o botão).
    fn aplicar_saidas(&self, c: &mut Cena, saidas: Vec<Saida>) {
        let mut passos = Vec::new();
        let mut redesenhar = false;
        for s in saidas {
            let raiz = c.janelas.foco();
            let popups = raiz.map(|r| c.janelas.popups_de(r)).unwrap_or_default();
            let raiz_info = raiz.and_then(|r| c.janelas.tamanho(r).map(|t| (r, t)));
            let cena = CenaEntrada { raiz: raiz_info, popups: &popups, origem: c.origem, composto: c.composto };
            let ef = c.mapeador.aplicar(s, &mut c.vista, &cena);
            redesenhar |= ef.redesenhar;
            if ef.vibrar {
                self.fila.empurrar(Evento::Vibrar(30));
            }
            for m in ef.mensagens {
                let apertou = matches!(m, Message::PointerButton { pressed: true, .. });
                passos.push(Passo::Msg(m));
                if apertou {
                    passos.push(Passo::PausaMs(PAUSA_BOTAO_MS));
                }
            }
        }
        self.enviar_passos(passos);
        if redesenhar {
            self.desenhar(c);
        }
    }

    pub fn toque(&self, t: &Toque) {
        let mut c = travar(&self.cena);
        let saidas = c.gestos.evento(t);
        self.aplicar_saidas(&mut c, saidas);
    }

    /// O relógio (toque longo): a casca o chama a cada quadro, pelo `poll`.
    pub fn tick(&self, agora_ms: u64) {
        let mut c = travar(&self.cena);
        let saidas = c.gestos.tick(agora_ms);
        if !saidas.is_empty() {
            self.aplicar_saidas(&mut c, saidas);
        }
    }

    fn janela_do_teclado(c: &Cena) -> Option<u32> {
        c.janelas.foco()
    }

    /// Texto do teclado virtual (`commitText`).
    pub fn texto(&self, texto: &str) {
        let mut c = travar(&self.cena);
        let Some(win) = Self::janela_do_teclado(&c) else { return };
        let passos = c.teclado.texto(win, texto);
        self.avisar_descartados(&mut c);
        self.enviar_passos(passos);
    }

    fn avisar_descartados(&self, c: &mut Cena) {
        let n = c.teclado.descartados_e_zerar();
        if n > 0 {
            let plural = if n == 1 { "caractere" } else { "caracteres" };
            self.fila.empurrar(Evento::Aviso(format!("{n} {plural} sem tecla no teclado ABNT2 {} descartado{}", if n == 1 { "foi" } else { "foram" }, if n == 1 { "" } else { "s" })));
        }
    }

    /// `deleteSurroundingText`.
    pub fn apagar(&self, antes: u32, depois: u32) {
        let mut c = travar(&self.cena);
        let Some(win) = Self::janela_do_teclado(&c) else { return };
        let passos = c.teclado.apagar(win, antes, depois);
        self.enviar_passos(passos);
    }

    /// Tecla da faixa (código evdev).
    pub fn tecla_extra(&self, code: u32) {
        let mut c = travar(&self.cena);
        let Some(win) = Self::janela_do_teclado(&c) else { return };
        let passos = c.teclado.tecla_extra(win, code);
        self.enviar_passos(passos);
    }

    /// Liga ou desliga Ctrl ou Alt grudentos; devolve o estado novo.
    pub fn alternar_modificador(&self, m: Mod) -> bool {
        travar(&self.cena).teclado.alternar(m)
    }

    /// (Ctrl, Alt) grudados agora.
    pub fn modificadores(&self) -> (bool, bool) {
        travar(&self.cena).teclado.grudados()
    }

    /// Tecla do teclado físico pelo `scanCode`.
    pub fn tecla_fisica(&self, scan_code: i32, apertada: bool) {
        let mut c = travar(&self.cena);
        let Some(win) = Self::janela_do_teclado(&c) else { return };
        let passos = c.teclado.tecla_fisica(win, scan_code, apertada);
        self.enviar_passos(passos);
    }

    /// Toque numa aba: passa o foco a essa janela.
    pub fn focar_aba(&self, id: u32) {
        let mut c = travar(&self.cena);
        let m = c.janelas.focar(id);
        self.aplicar_mudanca(&mut c, m);
    }

    /// O aplicativo ganhou ou perdeu o foco do sistema. Ao ganhar, `texto_da_area` é o que há na área de
    /// transferência do celular (a leitura só é permitida com o aplicativo em foco) e vai ao Pi se mudou.
    pub fn foco_do_app(&self, em_foco: bool, texto_da_area: Option<&str>) {
        let mut c = travar(&self.cena);
        c.app_em_foco = em_foco;
        if let Some(f) = c.janelas.foco() {
            self.mandar(Message::Focus { win: f, focused: em_foco });
        }
        drop(c);
        if em_foco {
            if let Some(t) = texto_da_area {
                *travar(&self.area.texto) = Some(t.to_string());
                self.clip.on_focus();
            }
        }
    }
}

impl UiSink for Sessao {
    fn send(&self, ev: UserEvent) -> bool {
        if self.parada() {
            return false;
        }
        self.tratar(ev);
        true
    }
}

// =============================================================================================================
// Controle
// =============================================================================================================

/// Onde ficam as chaves e como o cliente se chama.
#[derive(Clone)]
pub struct Config {
    /// `getFilesDir()`: guarda `aparelhos-conhecidos` (a chave do cliente é entregue pela casca).
    pub pasta: PathBuf,
    pub nome_cliente: String,
}

struct Interno {
    fila: Arc<Fila>,
    config: Config,
    tela: Mutex<TelaInfo>,
    /// Fator de escala escolhido na casca; vale para a próxima conexão e para a atual.
    fator: Mutex<f32>,
    superficie: SuperficieComum,
    sessao: Mutex<Option<Arc<Sessao>>>,
    /// Resposta da casca sobre a gravação da chave nova.
    gravada: (Mutex<Option<bool>>, Condvar),
    /// Cada conexão e cada busca recebem um número; um número velho cancela o que ficou para trás.
    geracao: AtomicU64,
    /// `estatisticas=1` (APK depurável): linhas VIDEOSTATS e AUDIOSTATS no `logcat`.
    estatisticas: AtomicBool,
    /// `semvideo=1` (APK depurável): não anuncia `CAP_H264` (a mesma tela segue pelo codec de blocos).
    sem_video: AtomicBool,
    fabrica_saida: Mutex<Option<FabricaDeSaida>>,
}

/// O que a casca controla. Barato de clonar.
#[derive(Clone)]
pub struct Controle {
    i: Arc<Interno>,
}

/// As linhas de estatística de um segundo: `VIDEOSTATS` (a janela de medição que se fecha aqui) e `AUDIOSTATS`, com os
/// mesmos campos do `psc`. Abre a janela seguinte.
pub fn linhas_de_estatisticas(stats: &Stats, audio: Option<&AudioEngine>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(r) = stats.window_report() {
        v.push(r.videostats_line());
    }
    stats.window_start();
    if let Some(a) = audio {
        v.push(a.stats().audiostats_line());
    }
    v
}

/// Escreve no `logcat` as linhas de estatística uma vez por segundo até a sessão acabar.
fn registrar_estatisticas(s: &Sessao, stats: &Stats, audio: Option<&AudioEngine>) {
    stats.window_start();
    while !s.parada() {
        std::thread::sleep(Duration::from_secs(1));
        for l in linhas_de_estatisticas(stats, audio) {
            registro::info(&l);
        }
    }
}

/// Procura aparelhos na rede (mDNS) e os entrega como eventos. Bloqueia por ~1,5 s.
pub fn buscar_agora(fila: &Fila) -> usize {
    let achados = discovery::search();
    let n = achados.len();
    for f in &achados {
        fila.empurrar(Evento::Aparelho {
            nome: f.instance.clone(),
            enderecos: f.addrs.iter().map(|a| format!("{a}:{}", f.port)).collect(),
            seguro: f.secure,
            pareamento_aberto: f.pair_open,
        });
    }
    n
}

/// `"192.168.0.13"`, `"192.168.0.13:7700"` ou um nome: o primeiro endereço IPv4 resolvido (porta padrão se faltar).
pub fn resolver(endereco: &str) -> Result<SocketAddr, String> {
    let e = endereco.trim();
    if e.is_empty() || e.len() > 255 || e.chars().any(char::is_control) {
        return Err("endereço vazio ou inválido".into());
    }
    let com_porta = if e.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) { e.to_string() } else { format!("{e}:{PORTA_PADRAO}") };
    let mut todos = com_porta.to_socket_addrs().map_err(|err| format!("não foi possível resolver \"{e}\": {err}"))?;
    todos.find(SocketAddr::is_ipv4).ok_or_else(|| format!("\"{e}\" não tem endereço IPv4"))
}

impl Controle {
    pub fn new(config: Config) -> Controle {
        Controle {
            i: Arc::new(Interno {
                fila: Arc::new(Fila::new()),
                config,
                tela: Mutex::new(TelaInfo { largura: 0, altura: 0, dpi: 440 }),
                fator: Mutex::new(FATOR_PADRAO),
                superficie: Arc::new(Mutex::new(None)),
                sessao: Mutex::new(None),
                gravada: (Mutex::new(None), Condvar::new()),
                geracao: AtomicU64::new(0),
                estatisticas: AtomicBool::new(false),
                sem_video: AtomicBool::new(false),
                fabrica_saida: Mutex::new(None),
            }),
        }
    }

    pub fn fila(&self) -> Arc<Fila> {
        self.i.fila.clone()
    }

    fn sessao(&self) -> Option<Arc<Sessao>> {
        travar(&self.i.sessao).clone()
    }

    fn nova_geracao(&self) -> u64 {
        self.i.geracao.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn vigente(&self, g: u64) -> bool {
        self.i.geracao.load(Ordering::SeqCst) == g
    }

    /// Procura aparelhos numa thread; avisa o fim com `BuscaFim`.
    pub fn buscar(&self) {
        let i = self.i.clone();
        let g = self.nova_geracao();
        let ctl = self.clone();
        std::thread::Builder::new()
            .name("psa-busca".into())
            .spawn(move || {
                i.fila.empurrar(Evento::Estado { fase: fase::BUSCANDO, texto: "Procurando aparelhos na rede…".into() });
                let n = buscar_agora(&i.fila);
                if ctl.vigente(g) {
                    i.fila.empurrar(Evento::BuscaFim { achados: n as u32 });
                    i.fila.empurrar(Evento::Estado { fase: fase::OCIOSO, texto: String::new() });
                }
            })
            .expect("criar thread de busca");
    }

    /// Conecta. `chave` são os 32 bytes da chave do cliente que a casca abriu do Keystore; `None` = ainda não há
    /// chave: o núcleo cria uma, entrega à casca no evento `ChaveNova` e espera `chave_gravada`.
    pub fn conectar(&self, endereco: &str, chave: Option<Box<Segredo>>) {
        self.desconectar();
        let g = self.nova_geracao();
        let ctl = self.clone();
        let endereco = endereco.to_string();
        std::thread::Builder::new()
            .name("psa-conexao".into())
            .spawn(move || {
                if let Err(e) = ctl.conectar_bloqueante(g, &endereco, chave) {
                    if ctl.vigente(g) {
                        registro::erro(&format!("conexão com {endereco} falhou: {e}"));
                        ctl.i.fila.empurrar(Evento::Erro(e));
                        ctl.i.fila.empurrar(Evento::Estado { fase: fase::ERRO, texto: String::new() });
                    }
                }
            })
            .expect("criar thread de conexão");
    }

    /// A casca avisa se gravou a chave nova (`true`) ou não conseguiu.
    pub fn chave_gravada(&self, ok: bool) {
        let (m, cv) = &self.i.gravada;
        *travar(m) = Some(ok);
        cv.notify_all();
    }

    fn esperar_chave_gravada(&self) -> Result<(), String> {
        let (m, cv) = &self.i.gravada;
        let fim = Instant::now() + TEMPO_CHAVE;
        let mut g = travar(m);
        loop {
            match g.take() {
                Some(true) => return Ok(()),
                Some(false) => return Err("não foi possível gravar a chave do cliente com a proteção do Android; nada foi feito".into()),
                None => {}
            }
            let agora = Instant::now();
            if agora >= fim {
                return Err("a casca não confirmou a gravação da chave do cliente a tempo".into());
            }
            g = cv.wait_timeout(g, fim - agora).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn conectar_bloqueante(&self, g: u64, endereco: &str, chave: Option<Box<Segredo>>) -> Result<(), String> {
        let fila = &self.i.fila;
        fila.empurrar(Evento::Estado { fase: fase::CONECTANDO, texto: format!("Conectando a {endereco}…") });
        let sa = resolver(endereco)?;

        // A chave primeiro: a conexão só abre depois de a chave existir (e, se é nova, estar gravada).
        let key = match chave {
            Some(b) => {
                // `Keypair` apaga o segredo ao ser solto; o `Segredo` da caixa se apaga aqui (e em qualquer retorno
                // antecipado abaixo, pelo `Drop`).
                let k = Keypair::from_secret(b.0);
                drop(b);
                k
            }
            None => {
                let k = Keypair::generate().map_err(|e| format!("não foi possível criar a chave do cliente: {e}"))?;
                *travar(&self.i.gravada.0) = None;
                let mut s = Box::new(Segredo([0u8; 32]));
                s.0.copy_from_slice(k.secret_bytes());
                fila.empurrar(Evento::ChaveNova(s));
                self.esperar_chave_gravada()?;
                k
            }
        };
        if !self.vigente(g) {
            return Ok(());
        }

        let tcp = TcpStream::connect_timeout(&sa, TEMPO_CONEXAO).and_then(TcpTransport::from_stream).map_err(|e| format!("não foi possível conectar a {sa}: {e}"))?;
        let fila2 = fila.clone();
        let so = SecurityOptions {
            // Nada em texto claro no celular: o canal cifrado vale até no laço local (docs/26, aceite 3).
            insecure: false,
            force_noise: true,
            client_name: self.i.config.nome_cliente.clone(),
            keys_dir: self.i.config.pasta.clone(),
            on_code: Some(Arc::new(move |codigo: &str| {
                fila2.empurrar(Evento::Codigo(codigo.to_string()));
                fila2.empurrar(Evento::Estado { fase: fase::PAREANDO, texto: "Confira o código no aparelho e aprove o pareamento".into() });
            })),
        };
        let conn = match secure::open_with_key(tcp, &sa.to_string(), &so, &key).map_err(|e| e.message().to_string())? {
            Connected::Secure(c) => *c,
            Connected::Plain(_) => return Err("conexão em texto claro recusada".into()),
        };
        drop(key);
        if !self.vigente(g) {
            return Ok(());
        }
        self.iniciar_sessao(g, conn, sa)
    }

    fn iniciar_sessao<T: Transport>(&self, g: u64, transport: T, sa: SocketAddr) -> Result<(), String> {
        let peer = transport.peer();
        let Split { mut reader, mut writer, closer } = transport.split().map_err(|e| format!("transporte: {e}"))?;
        // Vídeo e som só se anunciam se o aparelho os abre agora (como o `psc`): sem decodificador H.264 tudo segue pelo
        // codec de blocos; sem saída de som não há `CAP_AUDIO`.
        let h264 = match ps_client::video::probe().and_then(|()| if self.i.sem_video.load(Ordering::SeqCst) { Err(std::io::Error::other("desligado pelo extra de teste semvideo")) } else { Ok(()) }) {
            Ok(()) => true,
            Err(e) => {
                registro::info(&format!("vídeo H.264 desativado: {e}"));
                false
            }
        };
        let audio = self.abrir_audio();
        let caps = CLIENT_CAPS | CAP_ATOMIC | if h264 { CAP_H264 } else { 0 } | if audio.is_some() { CAP_AUDIO } else { 0 };
        let apagar_audio = |a: &Option<Arc<AudioEngine>>| {
            if let Some(a) = a {
                a.shutdown();
            }
        };
        if let Err(e) = write_message(&mut writer, &Message::hello_with_caps(&format!("psc-android {}", env!("CARGO_PKG_VERSION")), caps)) {
            apagar_audio(&audio);
            return Err(format!("falha ao enviar HELLO: {e}"));
        }
        let (r, expirou) = with_deadline(&closer, TEMPO_HELLO, || read_hello(&mut reader));
        let (_caps, nome) = match r {
            Ok(v) if !expirou => v,
            _ => {
                apagar_audio(&audio);
                return Err(format!("o aparelho {peer} não respondeu o HELLO a tempo"));
            }
        };

        let tela = *travar(&self.i.tela);
        let (out_tx, out_rx) = mpsc::channel::<Message>();
        let fator = *travar(&self.i.fator);
        let (sessao, entrada_rx) = Sessao::nova_com_fator(self.i.fila.clone(), out_tx.clone(), self.i.superficie.clone(), tela, fator, Some(closer), audio.clone());
        let ui: Arc<dyn UiSink> = sessao.clone();
        let stats = Arc::new(Stats::default());
        let t0 = Instant::now();
        let mic = ps_client::mic::Mic::new(None, false, out_tx.clone(), Arc::new(|_: &str| {}));
        net::spawn_writer(writer, out_rx, ui.clone());
        net::spawn_reader(reader, ui, out_tx.clone(), stats.clone(), t0, Registry::new(), sessao.clipboard(), h264, true, audio.clone(), mic);
        net::spawn_stats(out_tx, stats.clone(), t0, audio.clone());
        if self.i.estatisticas.load(Ordering::SeqCst) {
            let (s3, a3) = (sessao.clone(), audio.clone());
            std::thread::Builder::new().name("psa-estatisticas".into()).spawn(move || registrar_estatisticas(&s3, &stats, a3.as_deref())).expect("criar thread de estatísticas");
        }
        let s2 = sessao.clone();
        std::thread::Builder::new().name("psa-entrada".into()).spawn(move || s2.executar_entrada(entrada_rx)).expect("criar thread de entrada");

        if !self.vigente(g) {
            sessao.parar();
            return Ok(());
        }
        *travar(&self.i.sessao) = Some(sessao);
        registro::info(&format!("conectado a {sa} (\"{nome}\")"));
        self.i.fila.empurrar(Evento::Estado { fase: fase::CONECTADO, texto: format!("Conectado a \"{nome}\"") });
        Ok(())
    }

    /// Liga o registro de estatísticas no `logcat` (extra de depuração `estatisticas=1`).
    pub fn estatisticas(&self, ligado: bool) {
        self.i.estatisticas.store(ligado, Ordering::SeqCst);
    }

    /// Extra de teste: não anunciar o H.264 (o vídeo segue pelo codec de blocos).
    pub fn sem_video(&self, ligado: bool) {
        self.i.sem_video.store(ligado, Ordering::SeqCst);
    }

    /// Como o cliente abre a saída de som do sistema (no Android, o AAudio).
    pub fn definir_fabrica_de_saida(&self, f: FabricaDeSaida) {
        *travar(&self.i.fabrica_saida) = Some(f);
    }

    /// Abre o motor de áudio e a saída do sistema; `None` se não há fábrica ou ela falhou.
    fn abrir_audio(&self) -> Option<Arc<AudioEngine>> {
        let f = (*travar(&self.i.fabrica_saida))?;
        let engine = AudioEngine::new(None);
        match f(engine.clone()) {
            Ok(saida) => {
                registro::info(&format!("áudio: saída {}", saida.name()));
                engine.attach_sink(saida);
                Some(engine)
            }
            Err(e) => {
                registro::erro(&format!("áudio desativado: {e}"));
                None
            }
        }
    }

    /// Fecha a conexão (e cancela uma que ainda esteja abrindo).
    pub fn desconectar(&self) {
        self.nova_geracao();
        if let Some(s) = travar(&self.i.sessao).take() {
            s.parar();
        }
    }

    // ---- tela ------------------------------------------------------------------------------------------------

    /// A casca entrega a superfície (ou tira, com `None`). O desenho recomeça na hora.
    pub fn definir_superficie(&self, s: Option<Box<dyn Tela>>) {
        *travar(&self.i.superficie) = s;
        if let Some(se) = self.sessao() {
            se.redesenhar();
        }
    }

    pub fn definir_tela(&self, t: TelaInfo) {
        *travar(&self.i.tela) = t;
        if let Some(se) = self.sessao() {
            se.definir_tela(t);
        }
    }

    /// A casca escolheu o fator de escala em centésimos (250 = 2,5): valida, limita ao passo e aos limites, guarda para as
    /// próximas conexões e aplica na atual. Registra no `logcat` o fator e o pedido de tamanho que ele dá.
    pub fn escala(&self, centesimos: i32) {
        let fator = fator_de_centesimos(centesimos);
        *travar(&self.i.fator) = fator;
        let tela = *travar(&self.i.tela);
        let (w, h) = pedido_de_tamanho(&tela, fator, &Limites::default());
        registro::info(&format!("escala: {fator:.2} (pedido {w}x{h})"));
        if let Some(se) = self.sessao() {
            se.definir_fator(fator);
        }
    }

    // ---- entrada (sem sessão, ignorada) -----------------------------------------------------------------------

    pub fn toque(&self, t: &Toque) {
        if let Some(s) = self.sessao() {
            s.toque(t);
        }
    }

    pub fn texto(&self, t: &str) {
        if let Some(s) = self.sessao() {
            s.texto(t);
        }
    }

    pub fn apagar(&self, antes: u32, depois: u32) {
        if let Some(s) = self.sessao() {
            s.apagar(antes, depois);
        }
    }

    pub fn tecla_extra(&self, code: u32) {
        if let Some(s) = self.sessao() {
            s.tecla_extra(code);
        }
    }

    /// Alterna Ctrl ou Alt grudento; devolve o estado novo (`false` sem conexão).
    pub fn alternar_modificador(&self, m: Mod) -> bool {
        self.sessao().is_some_and(|s| s.alternar_modificador(m))
    }

    pub fn tecla_fisica(&self, scan_code: i32, apertada: bool) {
        if let Some(s) = self.sessao() {
            s.tecla_fisica(scan_code, apertada);
        }
    }

    /// (Ctrl, Alt) grudados agora (`(false, false)` sem conexão).
    pub fn modificadores(&self) -> (bool, bool) {
        self.sessao().map_or((false, false), |s| s.modificadores())
    }

    pub fn focar_aba(&self, id: u32) {
        if let Some(s) = self.sessao() {
            s.focar_aba(id);
        }
    }

    pub fn foco_do_app(&self, em_foco: bool, texto_da_area: Option<&str>) {
        if let Some(s) = self.sessao() {
            s.foco_do_app(em_foco, texto_da_area);
        }
    }

    /// O `nativePoll`: anda o relógio dos gestos e devolve os eventos pendentes no formato de `eventos.rs`.
    pub fn poll(&self, agora_ms: u64) -> Vec<u8> {
        if let Some(s) = self.sessao() {
            if s.parada() {
                *travar(&self.i.sessao) = None;
            } else {
                s.tick(agora_ms);
            }
        }
        self.i.fila.drenar_bytes()
    }
}
