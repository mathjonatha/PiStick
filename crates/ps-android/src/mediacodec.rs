
use std::ffi::{CStr, CString};
use std::io;
use std::time::{Duration, Instant};

use ps_client::video::VideoDecoder;

use crate::decodificador::{calcular_planos, converter, cor_de, Acao, Drenagem, FormatoSaida};
use crate::ndk::{self, AMediaCodec, AMediaCodecBufferInfo, AMediaFormat, AMEDIA_OK};

const ESPERA_ENTRADA: Duration = Duration::from_millis(200);
const US_POR_QUADRO: u64 = 33_333;

fn erro(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

pub struct DecodificadorAndroid {
    codec: *mut AMediaCodec,
    regiao: (u32, u32),
    formato: Option<FormatoSaida>,
    drenagem: Drenagem,
    quadros: u64,
}

unsafe impl Send for DecodificadorAndroid {}

unsafe fn ler_int(fmt: *mut AMediaFormat, nome: &CStr) -> Option<i32> {
    let mut v = 0i32;
    let ok = unsafe { ndk::AMediaFormat_getInt32(fmt, nome.as_ptr(), &mut v) };
    ok.then_some(v)
}

impl DecodificadorAndroid {
    pub fn abrir(largura: u32, altura: u32) -> io::Result<DecodificadorAndroid> {
        if largura == 0 || altura == 0 || largura > 8192 || altura > 8192 {
            return Err(erro(format!("região {largura}x{altura} fora do que o decodificador aceita")));
        }
        let mime = CString::new("video/avc").map_err(erro_nul)?;
        let codec = unsafe { ndk::AMediaCodec_createDecoderByType(mime.as_ptr()) };
        if codec.is_null() {
            return Err(erro("o aparelho não tem decodificador H.264 (video/avc)"));
        }
        let d = DecodificadorAndroid { codec, regiao: (largura, altura), formato: None, drenagem: Drenagem::new(), quadros: 0 };
        unsafe {
            let fmt = ndk::AMediaFormat_new();
            if fmt.is_null() {
                return Err(erro("AMediaFormat_new falhou"));
            }
            let chave = |s: &str| CString::new(s).expect("sem NUL");
            ndk::AMediaFormat_setString(fmt, chave("mime").as_ptr(), mime.as_ptr());
            ndk::AMediaFormat_setInt32(fmt, chave("width").as_ptr(), largura as i32);
            ndk::AMediaFormat_setInt32(fmt, chave("height").as_ptr(), altura as i32);
            ndk::AMediaFormat_setInt32(fmt, chave("color-format").as_ptr(), crate::decodificador::COLOR_FORMAT_YUV420_SEMI_PLANAR);
            ndk::AMediaFormat_setInt32(fmt, chave("low-latency").as_ptr(), 1);
            let r = ndk::AMediaCodec_configure(d.codec, fmt, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            ndk::AMediaFormat_delete(fmt);
            if r != AMEDIA_OK {
                return Err(erro(format!("AMediaCodec_configure falhou ({r})")));
            }
            let r = ndk::AMediaCodec_start(d.codec);
            if r != AMEDIA_OK {
                return Err(erro(format!("AMediaCodec_start falhou ({r})")));
            }
        }
        Ok(d)
    }

    fn ler_formato(&self) -> io::Result<FormatoSaida> {
        unsafe {
            let fmt = ndk::AMediaCodec_getOutputFormat(self.codec);
            if fmt.is_null() {
                return Err(erro("AMediaCodec_getOutputFormat devolveu nulo"));
            }
            let n = |s: &str| CString::new(s).expect("sem NUL");
            let ler = |s: &str| ler_int(fmt, &n(s));
            let r = (|| {
                let color_format = ler("color-format")?;
                Some(FormatoSaida {
                    largura: ler("width").map_or(self.regiao.0, |v| v.max(0) as u32),
                    altura: ler("height").map_or(self.regiao.1, |v| v.max(0) as u32),
                    stride: ler("stride").map(|v| v.max(0) as u32),
                    slice_height: ler("slice-height").map(|v| v.max(0) as u32),
                    crop_esq: ler("crop-left"),
                    crop_topo: ler("crop-top"),
                    color_format,
                    padrao_de_cor: ler("color-standard"),
                    faixa_de_cor: ler("color-range"),
                })
            })();
            ndk::AMediaFormat_delete(fmt);
            r.ok_or_else(|| erro("o formato de saída do decodificador não informa o formato de cor"))
        }
    }

    fn enfileirar(&mut self, data: &[u8]) -> io::Result<()> {
        let limite = Instant::now() + ESPERA_ENTRADA;
        let idx = loop {
            let i = unsafe { ndk::AMediaCodec_dequeueInputBuffer(self.codec, 5_000) };
            if i >= 0 {
                break i as usize;
            }
            if Instant::now() >= limite {
                return Err(erro("o decodificador não liberou buffer de entrada a tempo"));
            }
            self.descartar_saida();
        };
        let pts = self.quadros * US_POR_QUADRO;
        self.quadros += 1;
        unsafe {
            let mut cap = 0usize;
            let p = ndk::AMediaCodec_getInputBuffer(self.codec, idx, &mut cap);
            if p.is_null() || data.len() > cap {
                ndk::AMediaCodec_queueInputBuffer(self.codec, idx, 0, 0, pts, 0);
                return Err(erro(format!("o buffer de entrada ({cap} bytes) não comporta a unidade de acesso de {} bytes", data.len())));
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            let r = ndk::AMediaCodec_queueInputBuffer(self.codec, idx, 0, data.len(), pts, 0);
            if r != AMEDIA_OK {
                return Err(erro(format!("AMediaCodec_queueInputBuffer falhou ({r})")));
            }
        }
        Ok(())
    }

    fn descartar_saida(&mut self) {
        let mut info = AMediaCodecBufferInfo { offset: 0, size: 0, presentation_time_us: 0, flags: 0 };
        for _ in 0..16 {
            let r = unsafe { ndk::AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, 0) };
            if r >= 0 {
                unsafe { ndk::AMediaCodec_releaseOutputBuffer(self.codec, r as usize, false) };
            } else if r == -2 {
                self.formato = None;
            } else if r != -3 {
                break;
            }
        }
    }

    fn ler_saida(&mut self, idx: usize, info: &AMediaCodecBufferInfo, out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>> {
        let resultado = self.converter_saida(idx, info, out);
        unsafe { ndk::AMediaCodec_releaseOutputBuffer(self.codec, idx, false) };
        resultado
    }

    fn converter_saida(&mut self, idx: usize, info: &AMediaCodecBufferInfo, out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>> {
        if info.size <= 0 || info.offset < 0 {
            return Ok(None);
        }
        if self.formato.is_none() {
            self.formato = Some(self.ler_formato()?);
        }
        let f = self.formato.expect("acabou de ser lido");
        let buf: &[u8] = unsafe {
            let mut cap = 0usize;
            let p = ndk::AMediaCodec_getOutputBuffer(self.codec, idx, &mut cap);
            let (off, tam) = (info.offset as usize, info.size as usize);
            if p.is_null() || off.checked_add(tam).is_none_or(|fim| fim > cap) {
                return Err(erro(format!("o buffer de saída ({cap} bytes) é menor que offset {off} + tamanho {tam}")));
            }
            std::slice::from_raw_parts(p.add(off), tam)
        };
        let planos = calcular_planos(&f, buf.len(), self.regiao).map_err(erro)?;
        let cor = cor_de(&f, self.regiao.1);
        converter(&planos, buf, cor, out).map(Some).map_err(erro)
    }
}

fn erro_nul(e: std::ffi::NulError) -> io::Error {
    erro(e.to_string())
}

impl VideoDecoder for DecodificadorAndroid {
    fn decode(&mut self, data: &[u8], out: &mut Vec<u8>) -> io::Result<Option<(u32, u32)>> {
        if data.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unidade de acesso vazia"));
        }
        self.enfileirar(data)?;
        self.drenagem.comecar();
        let mut ultimo = None;
        let mut info = AMediaCodecBufferInfo { offset: 0, size: 0, presentation_time_us: 0, flags: 0 };
        loop {
            let r = unsafe { ndk::AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, self.drenagem.espera_us()) };
            match self.drenagem.acao(r) {
                Acao::Ler(i) => {
                    if let Some(tam) = self.ler_saida(i, &info, out)? {
                        ultimo = Some(tam);
                    }
                }
                Acao::LerFormato => self.formato = Some(self.ler_formato()?),
                Acao::Repetir => {}
                Acao::Terminar => break,
                Acao::Falha(m) => return Err(erro(m)),
            }
        }
        Ok(ultimo)
    }
}

impl Drop for DecodificadorAndroid {
    fn drop(&mut self) {
        unsafe {
            ndk::AMediaCodec_stop(self.codec);
            ndk::AMediaCodec_delete(self.codec);
        }
    }
}

pub fn abrir(largura: u32, altura: u32) -> io::Result<Box<dyn VideoDecoder>> {
    Ok(Box::new(DecodificadorAndroid::abrir(largura, altura)?))
}
