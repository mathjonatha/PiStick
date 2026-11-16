//! Decodificador H.264 do Media Foundation (só Windows): a MFT síncrona de software da Microsoft
//! (`CMSH264DecoderMFT`), em modo de baixa latência, entrada H.264 Annex B e saída NV12.
//!
//! **Todo o `unsafe` de COM do cliente está neste arquivo**: cada bloco `unsafe` tem um comentário
//! `// SAFETY:` com a invariante. As interfaces do crate `windows` contam referências sozinhas
//! (liberam no `Drop`), então não há `Release` à mão.
//!
//! O quadro que a MFT entrega pode ser maior que a região (o H.264 codifica múltiplos de 16 e sinaliza
//! o recorte no SPS): o tamanho de exibição vem da abertura mínima de exibição do tipo de saída
//! (`MF_MT_MINIMUM_DISPLAY_APERTURE`), e só esse retângulo é convertido e devolvido.

use std::io;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

use super::nv12::{nv12_to_bgrx, ColorSpec, Matrix, Range};
use super::VideoDecoder;

/// Duração de um quadro nas amostras, em unidades de 100 ns (30 por segundo).
const SAMPLE_DURATION: i64 = 333_333;

/// Quanto esperar, no máximo, pelo primeiro quadro de um decodificador recém-aberto quando o `ProcessOutput` responde
/// `NEED_MORE_INPUT` cedo demais (veja `MfDecoder::decode`). Medido: o quadro aparece em 1 a poucos ms, mesmo com a
/// máquina carregada; o prazo largo só existe para não perder o quadro numa máquina muito lenta.
const FIRST_FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

fn win_err(contexto: &str, e: windows::core::Error) -> io::Error {
    io::Error::other(format!("{contexto}: {e}"))
}

/// Formato de saída negociado com a MFT.
#[derive(Debug, Clone, Copy)]
struct OutFormat {
    /// Tamanho do quadro na memória (múltiplo de 16), em pixels.
    coded_w: u32,
    coded_h: u32,
    /// Retângulo de exibição: canto e tamanho.
    disp_x: u32,
    disp_y: u32,
    disp_w: u32,
    disp_h: u32,
    stride: usize,
    color: Option<ColorSpec>,
}

pub struct MfDecoder {
    transform: Option<IMFTransform>,
    out: OutFormat,
    provides_samples: bool,
    sample_size: u32,
    /// Largura e altura que o `VIDEO_START` anunciou (a MFT confere com o SPS).
    region: (u32, u32),
    frame_no: i64,
    /// Já saiu algum quadro deste decodificador (a espera de `FIRST_FRAME_WAIT` só vale até o primeiro).
    produced_any: bool,
    /// A cor que o decodificador de fato usou no último quadro (para o log e para os testes).
    last_color: Option<ColorSpec>,
    /// Força a cor (só para testes que provam que o critério de cor pega matriz ou faixa erradas).
    color_override: Option<ColorSpec>,
    /// Tem de ser o último campo: a interface `IMFTransform` (campo `transform`) é solta antes do `MFShutdown`.
    _session: MfSession,
}

/// Guarda de um `MFStartup` bem-sucedido: o `Drop` faz o `MFShutdown` correspondente, em qualquer
/// caminho (erro durante a abertura ou fim normal do decodificador).
struct MfSession;

impl Drop for MfSession {
    fn drop(&mut self) {
        // SAFETY: `MFShutdown` não recebe ponteiros e é o par do `MFStartup` que criou esta guarda
        // (uma guarda por `MFStartup`, solta uma única vez).
        unsafe {
            let _ = MFShutdown();
        }
    }
}

impl MfDecoder {
    pub fn open(width: u32, height: u32) -> io::Result<MfDecoder> {
        // SAFETY: inicializações sem ponteiros; `CoInitializeEx` pode devolver `S_FALSE` (já iniciado)
        // ou `RPC_E_CHANGED_MODE` (a thread já é STA): ambos servem, pois só usamos a MFT em processo.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| win_err("MFStartup", e))?;
        }
        // Daqui em diante todo retorno antecipado (`?`) desfaz o `MFStartup` pelo `Drop` da guarda.
        let session = MfSession;
        // SAFETY: `CoCreateInstance` devolve uma interface nova (ou erro); o CLSID é o da MFT H.264 da
        // Microsoft e o tipo pedido é `IMFTransform`.
        let transform: IMFTransform = unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }.map_err(|e| win_err("criar a MFT H.264", e))?;

        // SAFETY: todas as chamadas abaixo usam interfaces vivas (`transform` e os tipos de mídia
        // criados aqui), ponteiros para locais válidos da pilha e valores dentro dos tipos esperados.
        unsafe {
            // Baixa latência: um quadro de saída por unidade de acesso, sem fila de reordenação.
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let input = MFCreateMediaType().map_err(|e| win_err("tipo de entrada", e))?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| win_err("tipo principal", e))?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| win_err("subtipo H.264", e))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(width) << 32) | u64::from(height)).map_err(|e| win_err("tamanho", e))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| win_err("entrelaçamento", e))?;
            transform.SetInputType(0, &input, 0).map_err(|e| win_err("SetInputType", e))?;
        }
        let mut d = MfDecoder {
            transform: Some(transform),
            out: OutFormat { coded_w: width, coded_h: height, disp_x: 0, disp_y: 0, disp_w: width, disp_h: height, stride: width as usize, color: None },
            provides_samples: false,
            sample_size: 0,
            region: (width, height),
            frame_no: 0,
            produced_any: false,
            last_color: None,
            color_override: None,
            _session: session,
        };
        d.negotiate_output()?;
        let t = d.transform();
        // SAFETY: mensagens de controle sem ponteiros, na MFT viva.
        unsafe {
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(|e| win_err("BEGIN_STREAMING", e))?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(|e| win_err("START_OF_STREAM", e))?;
        }
        Ok(d)
    }

    /// Ignora a cor que a MFT informa e usa `c` (`None` volta ao normal).
    pub fn set_color_override(&mut self, c: Option<ColorSpec>) {
        self.color_override = c;
    }

    /// A matriz e a faixa que o último quadro usou (`None` até sair o primeiro).
    pub fn last_color(&self) -> Option<ColorSpec> {
        self.last_color
    }

    fn transform(&self) -> &IMFTransform {
        self.transform.as_ref().expect("a MFT só some no Drop")
    }

    /// Escolhe NV12 na saída e lê tamanho, recorte, passo e cor do tipo resultante. Chamada na abertura
    /// e a cada `MF_E_TRANSFORM_STREAM_CHANGE`.
    fn negotiate_output(&mut self) -> io::Result<()> {
        let t = self.transform().clone();
        // SAFETY: interfaces vivas; `GetBlob` recebe um buffer de 16 bytes (`MFVideoArea`) e `None`
        // para o tamanho devolvido; os valores lidos são inteiros simples.
        let (out, provides, size) = unsafe {
            let mut chosen = None;
            for i in 0.. {
                match t.GetOutputAvailableType(0, i) {
                    Ok(ty) => {
                        if ty.GetGUID(&MF_MT_SUBTYPE).map_err(|e| win_err("subtipo de saída", e))? == MFVideoFormat_NV12 {
                            chosen = Some(ty);
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let ty = chosen.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "a MFT H.264 não oferece saída NV12"))?;
            t.SetOutputType(0, &ty, 0).map_err(|e| win_err("SetOutputType", e))?;
            // Relê o tipo em vigor (a MFT pode ter completado campos).
            let ty = t.GetOutputCurrentType(0).map_err(|e| win_err("GetOutputCurrentType", e))?;
            let fs = ty.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| win_err("tamanho de saída", e))?;
            let (coded_w, coded_h) = ((fs >> 32) as u32, fs as u32);
            let (mut dx, mut dy, mut dw, mut dh) = (0u32, 0u32, coded_w, coded_h);
            let mut area = [0u8; 16];
            if ty.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, None).is_ok() {
                // MFVideoArea: MFOffset x { u16 fract, i16 value }, MFOffset y, SIZE { i32 cx, i32 cy }.
                let x = i16::from_le_bytes([area[2], area[3]]);
                let y = i16::from_le_bytes([area[6], area[7]]);
                let cx = i32::from_le_bytes([area[8], area[9], area[10], area[11]]);
                let cy = i32::from_le_bytes([area[12], area[13], area[14], area[15]]);
                if cx > 0 && cy > 0 && x >= 0 && y >= 0 {
                    (dx, dy, dw, dh) = (x as u32, y as u32, cx as u32, cy as u32);
                }
            }
            let stride = ty.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(coded_w as i32, |s| s as i32).unsigned_abs() as usize;
            let matrix = match ty.GetUINT32(&MF_MT_YUV_MATRIX).unwrap_or(0) {
                1 => Some(Matrix::Bt709),
                2 => Some(Matrix::Bt601),
                _ => None,
            };
            let range = match ty.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE).unwrap_or(0) {
                1 => Some(Range::Full),
                2 => Some(Range::Limited),
                _ => None,
            };
            let color = match (matrix, range) {
                (Some(matrix), Some(range)) => Some(ColorSpec { matrix, range }),
                (Some(matrix), None) => Some(ColorSpec { matrix, range: Range::Limited }),
                _ => None,
            };
            let info = t.GetOutputStreamInfo(0).map_err(|e| win_err("GetOutputStreamInfo", e))?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            (OutFormat { coded_w, coded_h, disp_x: dx, disp_y: dy, disp_w: dw, disp_h: dh, stride, color }, provides, info.cbSize)
        };
        self.out = out;
        self.provides_samples = provides;
        self.sample_size = size;
        Ok(())
    }

    /// Entrega uma unidade de acesso à MFT.
    fn feed(&mut self, data: &[u8]) -> io::Result<()> {
        let t = self.transform().clone();
        let time = self.frame_no * SAMPLE_DURATION;
        self.frame_no += 1;
        // SAFETY: `MFCreateMemoryBuffer` aloca `data.len()` bytes; `Lock` devolve um ponteiro para essa
        // memória, válida e exclusiva até o `Unlock`; copiamos exatamente `data.len()` bytes.
        unsafe {
            let buf = MFCreateMemoryBuffer(data.len() as u32).map_err(|e| win_err("buffer de entrada", e))?;
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None).map_err(|e| win_err("Lock da entrada", e))?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock().map_err(|e| win_err("Unlock da entrada", e))?;
            buf.SetCurrentLength(data.len() as u32).map_err(|e| win_err("tamanho da entrada", e))?;
            let sample = MFCreateSample().map_err(|e| win_err("amostra de entrada", e))?;
            sample.AddBuffer(&buf).map_err(|e| win_err("AddBuffer", e))?;
            sample.SetSampleTime(time).map_err(|e| win_err("SetSampleTime", e))?;
            sample.SetSampleDuration(SAMPLE_DURATION).map_err(|e| win_err("SetSampleDuration", e))?;
            t.ProcessInput(0, &sample, 0).map_err(|e| win_err("ProcessInput", e))
        }
    }

    /// Pede quadros à MFT até ela pedir mais entrada. Devolve o último que saiu.
    fn drain(&mut self, out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>> {
        let mut produced = None;
        loop {
            let t = self.transform().clone();
            // SAFETY: `MFT_OUTPUT_DATA_BUFFER` é preenchida com uma amostra nossa (ou vazia, se a MFT
            // fornece as amostras); depois do `ProcessOutput` a posse de `pSample` e `pEvents` volta
            // para nós e é tomada com `ManuallyDrop::take`, que a libera no fim do escopo.
            let result = unsafe {
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default()];
                if !self.provides_samples {
                    let sample = MFCreateSample().map_err(|e| win_err("amostra de saída", e))?;
                    let buf = MFCreateMemoryBuffer(self.sample_size.max(1)).map_err(|e| win_err("buffer de saída", e))?;
                    sample.AddBuffer(&buf).map_err(|e| win_err("AddBuffer da saída", e))?;
                    buffers[0].pSample = std::mem::ManuallyDrop::new(Some(sample));
                }
                let mut status = 0u32;
                let r = t.ProcessOutput(0, &mut buffers, &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                let _events = std::mem::ManuallyDrop::take(&mut buffers[0].pEvents);
                r.map(|()| sample)
            };
            match result {
                Ok(Some(sample)) => {
                    let size = self.convert(&sample, out)?;
                    produced = Some(size);
                }
                Ok(None) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(produced),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.negotiate_output()?,
                Err(e) => return Err(win_err("ProcessOutput", e)),
            }
        }
    }

    /// NV12 da amostra → BGRX do retângulo de exibição, em `out`.
    fn convert(&mut self, sample: &IMFSample, out: &mut Vec<u8>) -> io::Result<(u32, u32)> {
        let f = self.out;
        // SAFETY: `ConvertToContiguousBuffer` devolve um buffer vivo; `Lock` dá um ponteiro e o tamanho
        // atual dos dados, válidos e imutáveis até o `Unlock`; só lemos `len` bytes dentro desse intervalo.
        let converted = unsafe {
            let buf = sample.ConvertToContiguousBuffer().map_err(|e| win_err("ConvertToContiguousBuffer", e))?;
            let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
            buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| win_err("Lock da saída", e))?;
            let data = std::slice::from_raw_parts(p, len as usize);
            let r = self.nv12_to_display(data, f, out);
            let _ = buf.Unlock();
            r
        };
        converted?;
        Ok((f.disp_w, f.disp_h))
    }

    /// Parte pura da conversão: recorta o retângulo de exibição de um quadro NV12 em memória.
    fn nv12_to_display(&mut self, data: &[u8], f: OutFormat, out: &mut Vec<u8>) -> io::Result<()> {
        let (stride, coded_h) = (f.stride, f.coded_h as usize);
        let uv_base = stride * coded_h;
        if data.len() < uv_base + stride * coded_h / 2 || f.disp_x + f.disp_w > f.coded_w || f.disp_y + f.disp_h > f.coded_h {
            return Err(io::Error::other(format!("quadro NV12 incoerente: {} bytes para {}x{} com passo {stride}", data.len(), f.coded_w, f.coded_h)));
        }
        // O recorte começa em (disp_x, disp_y); para o plano UV os deslocamentos são pares e valem a metade.
        let (dx, dy) = (f.disp_x as usize & !1, f.disp_y as usize & !1);
        let (w, h) = (f.disp_w as usize & !1, f.disp_h as usize & !1);
        let y_plane = &data[dy * stride + dx..uv_base];
        let uv_plane = &data[uv_base + (dy / 2) * stride + dx..];
        let color = self.color_override.or(f.color).unwrap_or_else(|| ColorSpec::guess(self.region.1));
        self.last_color = Some(color);
        out.clear();
        out.resize(w * h * 4, 0);
        if !nv12_to_bgrx(y_plane, stride, uv_plane, stride, w, h, color, out) {
            return Err(io::Error::other("conversão NV12 recusou o quadro"));
        }
        Ok(())
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>> {
        if data.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unidade de acesso vazia"));
        }
        self.feed(data)?;
        let mut produced = self.drain(out)?;
        if produced.is_none() && !self.produced_any {
            // A MFT decodifica em threads próprias. No primeiro quadro de um decodificador recém-aberto, com a
            // máquina carregada, o `ProcessOutput` às vezes responde `NEED_MORE_INPUT` antes de a decodificação
            // terminar (nos casos sem saída ele voltava em ~0,1 ms, contra 8 a 30 ms quando espera). O quadro não
            // se perde: sem esta espera ele saía junto com o seguinte, atrasado de um quadro, e uma unidade de
            // acesso que devia dar um quadro dava zero. Só no primeiro quadro: depois dele a MFT nunca atrasou
            // (centenas de execuções sob carga), e uma unidade que de fato não produz imagem (só SPS e PPS, dado
            // inválido) não pode custar espera a cada vez. `DRAIN` não serve: o decodificador passa a descartar
            // tudo até o próximo IDR.
            let limite = std::time::Instant::now() + FIRST_FRAME_WAIT;
            while produced.is_none() && std::time::Instant::now() < limite {
                std::thread::sleep(std::time::Duration::from_millis(1));
                produced = self.drain(out)?;
            }
        }
        if produced.is_some() {
            self.produced_any = true;
        }
        Ok(produced)
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        if let Some(t) = self.transform.take() {
            // SAFETY: mensagem de encerramento sem ponteiros na MFT ainda viva. Depois soltamos a
            // interface; o `MFShutdown` fica com `MfSession`, o último campo.
            unsafe {
                let _ = t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            }
            drop(t);
        }
    }
}
