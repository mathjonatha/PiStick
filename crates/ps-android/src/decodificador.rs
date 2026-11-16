
use std::time::Duration;

use ps_client::video::nv12::{i420_to_bgrx, nv12_to_bgrx, ColorSpec, Matrix, Range};

pub const COLOR_FORMAT_YUV420_PLANAR: i32 = 19;
pub const COLOR_FORMAT_YUV420_SEMI_PLANAR: i32 = 21;

pub const ESPERA_PRIMEIRA: Duration = Duration::from_millis(500);
pub const ESPERA_NORMAL: Duration = Duration::from_millis(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retorno {
    Buffer(usize),
    TentarDepois,
    FormatoMudou,
    BuffersMudaram,
    Erro(isize),
}

pub fn classificar(r: isize) -> Retorno {
    match r {
        r if r >= 0 => Retorno::Buffer(r as usize),
        -1 => Retorno::TentarDepois,
        -2 => Retorno::FormatoMudou,
        -3 => Retorno::BuffersMudaram,
        e => Retorno::Erro(e),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acao {
    Ler(usize),
    LerFormato,
    Repetir,
    Terminar,
    Falha(String),
}

#[derive(Debug, Default)]
pub struct Drenagem {
    chamadas: u64,
    leituras: u32,
}

impl Drenagem {
    pub fn new() -> Drenagem {
        Drenagem::default()
    }

    pub fn comecar(&mut self) {
        self.chamadas += 1;
        self.leituras = 0;
    }

    pub fn espera_us(&self) -> i64 {
        if self.leituras > 0 {
            0
        } else if self.chamadas <= 1 {
            ESPERA_PRIMEIRA.as_micros() as i64
        } else {
            ESPERA_NORMAL.as_micros() as i64
        }
    }

    pub fn acao(&mut self, r: isize) -> Acao {
        match classificar(r) {
            Retorno::Buffer(i) => {
                self.leituras += 1;
                Acao::Ler(i)
            }
            Retorno::TentarDepois => Acao::Terminar,
            Retorno::FormatoMudou => Acao::LerFormato,
            Retorno::BuffersMudaram => Acao::Repetir,
            Retorno::Erro(e) => Acao::Falha(format!("dequeueOutputBuffer devolveu o erro {e}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatoSaida {
    pub largura: u32,
    pub altura: u32,
    pub stride: Option<u32>,
    pub slice_height: Option<u32>,
    pub crop_esq: Option<i32>,
    pub crop_topo: Option<i32>,
    pub color_format: i32,
    pub padrao_de_cor: Option<i32>,
    pub faixa_de_cor: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pixel {
    Nv12,
    I420,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planos {
    pub pixel: Pixel,
    pub stride: usize,
    pub y_off: usize,
    pub u_off: usize,
    pub u_stride: usize,
    pub v_off: usize,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub fn calcular_planos(f: &FormatoSaida, tam: usize, regiao: (u32, u32)) -> Result<Planos, String> {
    let pixel = match f.color_format {
        COLOR_FORMAT_YUV420_SEMI_PLANAR => Pixel::Nv12,
        COLOR_FORMAT_YUV420_PLANAR => Pixel::I420,
        outro => return Err(format!("formato de cor de saída não suportado: {outro} (só NV12 = 21 e I420 = 19)")),
    };
    let stride = f.stride.unwrap_or(f.largura).max(f.largura) as usize;
    let slice = f.slice_height.unwrap_or(f.altura).max(f.altura) as usize;
    if stride == 0 || slice == 0 {
        return Err("formato de saída com tamanho zero".into());
    }
    let (w, h) = (regiao.0 as usize, regiao.1 as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(format!("região {w}x{h}: a conversão de cor exige largura e altura pares e não nulas"));
    }
    let x = (f.crop_esq.unwrap_or(0).max(0) as usize) & !1;
    let y = (f.crop_topo.unwrap_or(0).max(0) as usize) & !1;
    if x + w > f.largura as usize || y + h > f.altura as usize {
        return Err(format!("a região {w}x{h} em ({x}, {y}) não cabe no quadro decodificado de {}x{}", f.largura, f.altura));
    }
    let y_tam = stride.checked_mul(slice).ok_or("tamanho do plano Y estoura")?;
    let (u_stride, u_off, v_off, fim) = match pixel {
        Pixel::Nv12 => {
            let ultima = y_tam + ((y + h) / 2 - 1) * stride + x + w;
            (stride, y_tam, 0, ultima)
        }
        Pixel::I420 => {
            let cs = stride / 2;
            let c_tam = cs * (slice / 2);
            let ultima_u = y_tam + ((y + h) / 2 - 1) * cs + x / 2 + w / 2;
            let ultima_v = y_tam + c_tam + ((y + h) / 2 - 1) * cs + x / 2 + w / 2;
            (cs, y_tam, y_tam + c_tam, ultima_u.max(ultima_v))
        }
    };
    let ultima_y = (y + h - 1) * stride + x + w;
    let preciso = ultima_y.max(fim);
    if tam < preciso {
        return Err(format!("o buffer de saída tem {tam} bytes e o formato anunciado ({}x{}, stride {stride}, slice-height {slice}) precisa de pelo menos {preciso}", f.largura, f.altura));
    }
    Ok(Planos { pixel, stride, y_off: 0, u_off, u_stride, v_off, x, y, w, h })
}

pub fn cor_de(f: &FormatoSaida, altura: u32) -> ColorSpec {
    let chute = ColorSpec::guess(altura);
    let matrix = match f.padrao_de_cor {
        Some(1) => Matrix::Bt709,
        Some(2) | Some(4) => Matrix::Bt601,
        _ => chute.matrix,
    };
    let range = match f.faixa_de_cor {
        Some(1) => Range::Full,
        Some(2) => Range::Limited,
        _ => chute.range,
    };
    ColorSpec { matrix, range }
}

pub fn converter(p: &Planos, buf: &[u8], cor: ColorSpec, out: &mut Vec<u8>) -> Result<(u32, u32), String> {
    out.clear();
    out.resize(p.w * p.h * 4, 0);
    let y = buf.get(p.y_off + p.y * p.stride + p.x..).ok_or("plano Y fora do buffer")?;
    let ok = match p.pixel {
        Pixel::Nv12 => {
            let uv = buf.get(p.u_off + (p.y / 2) * p.stride + p.x..).ok_or("plano UV fora do buffer")?;
            nv12_to_bgrx(y, p.stride, uv, p.stride, p.w, p.h, cor, out)
        }
        Pixel::I420 => {
            let u = buf.get(p.u_off + (p.y / 2) * p.u_stride + p.x / 2..).ok_or("plano U fora do buffer")?;
            let v = buf.get(p.v_off + (p.y / 2) * p.u_stride + p.x / 2..).ok_or("plano V fora do buffer")?;
            i420_to_bgrx(y, p.stride, u, p.u_stride, v, p.u_stride, p.w, p.h, cor, out)
        }
    };
    if ok {
        Ok((p.w as u32, p.h as u32))
    } else {
        Err("a conversão de cor recusou o quadro (tamanho incoerente)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn formato(cf: i32, larg: u32, alt: u32, stride: Option<u32>, slice: Option<u32>) -> FormatoSaida {
        FormatoSaida { largura: larg, altura: alt, stride, slice_height: slice, crop_esq: None, crop_topo: None, color_format: cf, padrao_de_cor: None, faixa_de_cor: None }
    }

    fn nv12(larg: usize, alt: usize, stride: usize, slice: usize, y: u8, u: u8, v: u8) -> Vec<u8> {
        let mut b = vec![0xEEu8; stride * slice + stride * (slice / 2)];
        for r in 0..alt {
            for x in 0..larg {
                b[r * stride + x] = y;
            }
        }
        for r in 0..alt / 2 {
            for x in 0..larg / 2 {
                b[stride * slice + r * stride + x * 2] = u;
                b[stride * slice + r * stride + x * 2 + 1] = v;
            }
        }
        b
    }

    #[test]
    fn retornos_do_codec_viram_acoes() {
        let mut d = Drenagem::new();
        d.comecar();
        assert_eq!(d.acao(3), Acao::Ler(3));
        assert_eq!(d.acao(-1), Acao::Terminar);
        assert_eq!(d.acao(-2), Acao::LerFormato);
        assert_eq!(d.acao(-3), Acao::Repetir);
        assert!(matches!(d.acao(-10000), Acao::Falha(m) if m.contains("-10000")));
        assert_eq!(classificar(0), Retorno::Buffer(0));
    }

    #[test]
    fn so_a_primeira_chamada_espera_o_tempo_longo_e_a_leitura_seguinte_nao_espera() {
        let mut d = Drenagem::new();
        d.comecar();
        assert_eq!(d.espera_us(), 500_000);
        assert_eq!(d.acao(-2), Acao::LerFormato);
        assert_eq!(d.espera_us(), 500_000);
        assert_eq!(d.acao(0), Acao::Ler(0));
        assert_eq!(d.espera_us(), 0, "depois do primeiro quadro, sem esperar mais");
        d.comecar();
        assert_eq!(d.espera_us(), 15_000, "chamadas seguintes: prazo curto, mesmo antes de sair o primeiro quadro");
        assert_eq!(d.acao(-1), Acao::Terminar);
    }

    #[test]
    fn planos_do_nv12_com_stride_e_slice_height_maiores_e_recorte() {
        let f = formato(21, 1280, 736, Some(1280), Some(736));
        let p = calcular_planos(&f, 1280 * 736 * 3 / 2, (1280, 720)).unwrap();
        assert_eq!((p.pixel, p.stride, p.u_off, p.u_stride, p.w, p.h, p.x, p.y), (Pixel::Nv12, 1280, 1280 * 736, 1280, 1280, 720, 0, 0));
        let f = FormatoSaida { crop_esq: Some(3), crop_topo: Some(5), ..formato(21, 640, 400, Some(704), Some(416)) };
        let p = calcular_planos(&f, 704 * 416 * 3 / 2, (320, 200)).unwrap();
        assert_eq!((p.x, p.y, p.w, p.h, p.stride), (2, 4, 320, 200, 704));
    }

    #[test]
    fn planos_do_i420_tem_u_e_v_com_meio_stride() {
        let f = formato(19, 64, 48, Some(64), Some(48));
        let p = calcular_planos(&f, 64 * 48 * 3 / 2, (64, 48)).unwrap();
        assert_eq!((p.pixel, p.u_stride, p.u_off, p.v_off), (Pixel::I420, 32, 64 * 48, 64 * 48 + 32 * 24));
    }

    #[test]
    fn sem_stride_e_slice_height_vale_a_largura_e_a_altura() {
        let f = formato(21, 64, 48, None, None);
        let p = calcular_planos(&f, 64 * 48 * 3 / 2, (64, 48)).unwrap();
        assert_eq!((p.stride, p.u_off), (64, 64 * 48));
        let f = formato(21, 64, 48, Some(10), Some(1));
        assert_eq!(calcular_planos(&f, 64 * 48 * 3 / 2, (64, 48)).unwrap().stride, 64);
    }

    #[test]
    fn buffer_menor_que_o_anunciado_e_erro_nunca_leitura_fora() {
        let f = formato(21, 1280, 736, Some(1280), Some(736));
        for tam in [0usize, 10, 1280 * 720, 1280 * 736, 1280 * 736 + 1280 * 359] {
            let e = calcular_planos(&f, tam, (1280, 720)).expect_err("buffer curto");
            assert!(e.contains("precisa de pelo menos"), "{tam}: {e}");
        }
        assert!(calcular_planos(&f, 1280 * 736 + 1280 * 359 + 1280, (1280, 720)).is_ok());
        let f = formato(19, 64, 48, Some(64), Some(48));
        assert!(calcular_planos(&f, 64 * 48, (64, 48)).is_err());
    }

    #[test]
    fn formato_nao_suportado_regiao_impar_e_regiao_fora_do_quadro_dao_erro_claro() {
        let e = calcular_planos(&formato(0x7F420888, 64, 48, None, None), 100_000, (64, 48)).expect_err("flexível");
        assert!(e.contains("não suportado") && e.contains("2135033992"), "{e}");
        assert!(calcular_planos(&formato(0x7FA30C04, 64, 48, None, None), 100_000, (64, 48)).is_err(), "formato próprio de fabricante");
        assert!(calcular_planos(&formato(21, 64, 48, None, None), 100_000, (63, 48)).unwrap_err().contains("pares"));
        assert!(calcular_planos(&formato(21, 64, 48, None, None), 100_000, (0, 48)).is_err());
        assert!(calcular_planos(&formato(21, 64, 48, None, None), 100_000, (66, 48)).unwrap_err().contains("não cabe"));
        assert!(calcular_planos(&FormatoSaida { crop_esq: Some(40), ..formato(21, 64, 48, None, None) }, 100_000, (32, 48)).is_err());
        assert!(calcular_planos(&formato(21, 0, 0, None, None), 100_000, (64, 48)).is_err());
    }

    #[test]
    fn conversao_nv12_com_stride_maior_que_a_largura_ignora_o_lixo_do_alinhamento() {
        let (larg, alt, stride, slice) = (8usize, 4usize, 16usize, 8usize);
        let buf = nv12(larg, alt, stride, slice, 180, 128, 128);
        let f = formato(21, larg as u32, alt as u32, Some(stride as u32), Some(slice as u32));
        let p = calcular_planos(&f, buf.len(), (8, 4)).unwrap();
        let mut out = Vec::new();
        let cor = ColorSpec { matrix: Matrix::Bt709, range: Range::Limited };
        assert_eq!(converter(&p, &buf, cor, &mut out).unwrap(), (8, 4));
        assert_eq!(out.len(), 8 * 4 * 4);
        let px0 = [out[0], out[1], out[2], out[3]];
        assert!(px0[0] == px0[1] && px0[1] == px0[2] && px0[3] == 255);
        assert!(out.chunks(4).all(|q| q == px0));
    }

    #[test]
    fn conversao_i420_e_nv12_dao_a_mesma_imagem_e_o_recorte_escolhe_o_pedaco() {
        let (larg, alt) = (8usize, 6usize);
        let mut y = vec![0u8; larg * alt];
        for r in 0..alt {
            for x in 0..larg {
                y[r * larg + x] = (40 + x * 10 + r) as u8;
            }
        }
        let mut b_nv12 = y.clone();
        let mut b_i420 = y.clone();
        for _ in 0..alt / 2 {
            for _ in 0..larg / 2 {
                b_nv12.extend_from_slice(&[100, 160]);
            }
        }
        b_i420.extend(std::iter::repeat_n(100u8, larg / 2 * alt / 2));
        b_i420.extend(std::iter::repeat_n(160u8, larg / 2 * alt / 2));
        let mk = |cf| FormatoSaida { crop_esq: Some(2), crop_topo: Some(2), ..formato(cf, larg as u32, alt as u32, None, None) };
        let cor = ColorSpec { matrix: Matrix::Bt601, range: Range::Full };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let pa = calcular_planos(&mk(21), b_nv12.len(), (4, 2)).unwrap();
        let pb = calcular_planos(&mk(19), b_i420.len(), (4, 2)).unwrap();
        assert_eq!(converter(&pa, &b_nv12, cor, &mut a).unwrap(), (4, 2));
        assert_eq!(converter(&pb, &b_i420, cor, &mut b).unwrap(), (4, 2));
        assert_eq!(a, b);
        let esperado = {
            let (yy, u, v) = (62.0f64, 100.0 - 128.0, 160.0 - 128.0);
            [(yy + 1.772 * u).round(), (yy - 0.344136 * u - 0.714136 * v).round(), (yy + 1.402 * v).round()]
        };
        for (i, e) in esperado.iter().enumerate() {
            assert!((f64::from(a[i]) - e.clamp(0.0, 255.0)).abs() <= 1.0, "canal {i}: {} contra {e}", a[i]);
        }
    }

    #[test]
    fn conversao_com_buffer_curto_demais_para_o_plano_da_erro_e_nao_panica() {
        let f = formato(21, 8, 4, Some(16), Some(8));
        let buf = nv12(8, 4, 16, 8, 100, 128, 128);
        let p = calcular_planos(&f, buf.len(), (8, 4)).unwrap();
        let mut out = Vec::new();
        let cor = ColorSpec::guess(4);
        assert!(converter(&p, &buf[..20], cor, &mut out).is_err());
        assert!(converter(&p, &[], cor, &mut out).is_err());
    }

    #[test]
    fn cor_vem_do_decodificador_e_cai_na_altura() {
        let base = formato(21, 64, 48, None, None);
        assert_eq!(cor_de(&base, 720), ColorSpec { matrix: Matrix::Bt709, range: Range::Limited });
        assert_eq!(cor_de(&base, 480), ColorSpec { matrix: Matrix::Bt601, range: Range::Limited });
        let f = FormatoSaida { padrao_de_cor: Some(2), faixa_de_cor: Some(1), ..base };
        assert_eq!(cor_de(&f, 720), ColorSpec { matrix: Matrix::Bt601, range: Range::Full });
        let f = FormatoSaida { padrao_de_cor: Some(1), faixa_de_cor: Some(2), ..base };
        assert_eq!(cor_de(&f, 240), ColorSpec { matrix: Matrix::Bt709, range: Range::Limited });
        let f = FormatoSaida { padrao_de_cor: Some(6), ..base };
        assert_eq!(cor_de(&f, 720).matrix, Matrix::Bt709, "BT.2020 não é suportada: cai no chute");
    }
}
