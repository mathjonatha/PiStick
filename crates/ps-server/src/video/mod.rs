//! Vídeo por região (marco 4, veja `docs/13-marco-4-video.md`): uma região retangular de uma
//! janela que se comporta como vídeo é transmitida em H.264 enquanto o resto continua no codec
//! de blocos.
//!
//! - [`detect`]: decide quando uma janela entra e sai do modo vídeo (lógica pura, sem relógio).
//! - [`VideoEncoder`] e [`VideoEncoderFactory`]: o que a sessão precisa de um codificador H.264.
//!   Nos testes entra um codificador falso; no `psd` em Linux, o V4L2 (etapa seguinte).

use std::io;

pub mod annexb;
pub mod detect;
#[cfg(target_os = "linux")]
pub mod v4l2;

/// Quadros de vídeo por segundo (limite máximo; o intervalo mínimo entre capturas é
/// [`MIN_CAPTURE_INTERVAL_MS`]).
pub const VIDEO_FPS: u32 = 30;
/// Intervalo mínimo entre duas capturas da região, em milissegundos.
pub const MIN_CAPTURE_INTERVAL_MS: u64 = 33;
/// Taxa de bits pedida ao codificador quando nada diz o contrário.
pub const DEFAULT_BITRATE: u32 = 5_000_000;
/// QP máximo padrão do H.264 (veja `--video-max-qp`; 51 = sem teto). Escolhido por medição no Wi-Fi com o
/// `mpv` (`docs/medicoes.md`, "Qualidade do H.264"): com 32 o fluxo (7 Mbit/s) derruba o vídeo para 27 quadros
/// por segundo; com 36 ficam 29 quadros por segundo (4,9 Mbit/s) e os degradês saem sem blocagem; com 51 (3,1
/// Mbit/s) os degradês saem com listras e manchas em blocos.
pub const DEFAULT_MAX_QP: u32 = 36;

/// Parâmetros de um fluxo H.264 (a região inteira, em pixels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    /// Bits por segundo.
    pub bitrate: u32,
    pub fps: u32,
    /// Teto de QP do H.264 (1 a 51); quanto menor, melhor a qualidade e mais bits.
    pub max_qp: u32,
}

/// Uma unidade de acesso H.264 em Annex B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// Quadro-chave (IDR, com SPS e PPS). O primeiro de todo fluxo tem de ser.
    pub keyframe: bool,
}

/// Codificador de um fluxo. Vive enquanto a região é vídeo; fechá-lo é soltá-lo.
pub trait VideoEncoder: Send {
    /// `pixels`: BGRX da região, `stride` em bytes (a primeira linha da região está no início de
    /// `pixels`). Bloqueia até o quadro sair, no máximo 200 ms: passado o prazo devolve erro
    /// (`ErrorKind::TimedOut`). Um erro encerra o fluxo.
    fn encode(&mut self, pixels: &[u8], stride: usize) -> io::Result<EncodedFrame>;

    /// Linha de log com o que o codificador de fato aplicou (controles lidos de volta); `None` se não há o que dizer.
    fn describe(&self) -> Option<String> {
        None
    }
}

/// Abre codificadores. O servidor só anuncia `CAP_H264` se tiver uma fábrica.
pub trait VideoEncoderFactory: Send + Sync {
    fn open(&self, cfg: &VideoConfig) -> io::Result<Box<dyn VideoEncoder>>;
}
