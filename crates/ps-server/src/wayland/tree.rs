
use std::collections::HashMap;
use std::hash::Hash;

use ps_codec::Rect;

pub(crate) type Ret = (i32, i32, i32, i32);

pub(crate) fn intersecta(a: Ret, b: Ret) -> Option<Ret> {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = a.0.saturating_add(a.2).min(b.0.saturating_add(b.2));
    let y1 = a.1.saturating_add(a.3).min(b.1.saturating_add(b.3));
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

#[derive(Default)]
pub(crate) struct Layer {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
    pub alpha: bool,
    pub lw: u32,
    pub lh: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Item<K> {
    pub key: K,
    pub root: bool,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl<K> Item<K> {
    fn ret(&self) -> Ret {
        (self.x, self.y, self.w as i32, self.h as i32)
    }
}

pub(crate) struct Tree<K> {
    pub root: Vec<u8>,
    pub root_w: u32,
    pub root_h: u32,
    pub root_alpha: bool,
    pub root_logical: (u32, u32),
    pub root_src: Option<Layer>,
    pub root_dst: Option<(i32, i32)>,
    pub sig: Vec<Item<K>>,
}

impl<K> Tree<K> {
    pub fn vazia() -> Tree<K> {
        Tree { root: Vec::new(), root_w: 0, root_h: 0, root_alpha: false, root_logical: (0, 0), root_src: None, root_dst: None, sig: Vec::new() }
    }

    pub fn de_janela(pixels: &[u8], w: u32, h: u32, alpha: bool, logical: (u32, u32)) -> Tree<K> {
        Tree { root: pixels.to_vec(), root_w: w, root_h: h, root_alpha: alpha, root_logical: logical, root_src: None, root_dst: None, sig: Vec::new() }
    }
}

pub(crate) struct Dirty<K> {
    pub subs: Vec<(K, Option<Vec<Rect>>)>,
    pub all: bool,
}

impl<K> Default for Dirty<K> {
    fn default() -> Self {
        Dirty { subs: Vec::new(), all: false }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mudanca {
    Igual,
    Areas(Vec<Ret>),
    Tudo,
}

pub(crate) fn compara<K: PartialEq>(velho: &[Item<K>], novo: &[Item<K>]) -> Mudanca {
    if velho == novo {
        return Mudanca::Igual;
    }
    let comuns_v: Vec<&K> = velho.iter().filter(|o| novo.iter().any(|n| n.key == o.key)).map(|o| &o.key).collect();
    let comuns_n: Vec<&K> = novo.iter().filter(|n| velho.iter().any(|o| o.key == n.key)).map(|n| &n.key).collect();
    if comuns_v != comuns_n {
        return Mudanca::Tudo;
    }
    let mut areas = Vec::new();
    areas.extend(velho.iter().filter(|o| !novo.contains(o)).map(Item::ret));
    areas.extend(novo.iter().filter(|n| !velho.contains(n)).map(Item::ret));
    Mudanca::Areas(areas)
}

pub(crate) fn mistura(d: &mut [u8], s: &[u8]) {
    let a = u32::from(s[3]);
    match a {
        255 => d[..4].copy_from_slice(&s[..4]),
        0 => {}
        _ => {
            for i in 0..4 {
                d[i] = (u32::from(s[i]) + (u32::from(d[i]) * (255 - a) + 127) / 255).min(255) as u8;
            }
        }
    }
}

fn copia_ou_mistura(dst: &mut [u8], src: &[u8], alpha: bool) {
    if alpha {
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            mistura(d, s);
        }
    } else {
        dst.copy_from_slice(src);
    }
}

fn desenha_layer(l: &Layer, (ix, iy, iw, ih): Ret, r: Ret, stride: usize, pixels: &mut [u8]) {
    if iw <= 0 || ih <= 0 || l.w == 0 || l.h == 0 || l.px.len() != l.w as usize * l.h as usize * 4 {
        return;
    }
    let Some((x0, y0, w, h)) = intersecta(r, (ix, iy, iw, ih)) else { return };
    if l.w as i32 == iw && l.h as i32 == ih {
        for y in y0..y0 + h {
            let s = ((y - iy) as usize * l.w as usize + (x0 - ix) as usize) * 4;
            let d = y as usize * stride + x0 as usize * 4;
            let n = w as usize * 4;
            copia_ou_mistura(&mut pixels[d..d + n], &l.px[s..s + n], l.alpha);
        }
    } else {
        for y in y0..y0 + h {
            let sy = ((y - iy) as usize * l.h as usize / ih as usize).min(l.h as usize - 1);
            for x in x0..x0 + w {
                let sx = ((x - ix) as usize * l.w as usize / iw as usize).min(l.w as usize - 1);
                let s = (sy * l.w as usize + sx) * 4;
                let d = y as usize * stride + x as usize * 4;
                copia_ou_mistura(&mut pixels[d..d + 4], &l.px[s..s + 4], l.alpha);
            }
        }
    }
}

pub(crate) fn compoe<K: Eq + Hash>(tree: &Tree<K>, layers: &HashMap<K, Layer>, origin: (i32, i32), (ww, wh): (u32, u32), pixels: &mut [u8], rects: &[Rect]) {
    let stride = ww as usize * 4;
    if pixels.len() < stride * wh as usize {
        return;
    }
    for r in rects {
        let (rx, ry, rw, rh) = (r.x as usize, r.y as usize, r.w as usize, r.h as usize);
        if rw == 0 || rh == 0 || rx + rw > ww as usize || ry + rh > wh as usize {
            continue;
        }
        for row in ry..ry + rh {
            pixels[row * stride + rx * 4..row * stride + (rx + rw) * 4].fill(0);
        }
        let area = (rx as i32, ry as i32, rw as i32, rh as i32);
        for it in &tree.sig {
            if it.root {
                for row in ry..ry + rh {
                    if row >= tree.root_h as usize || rx + rw > tree.root_w as usize || tree.root.len() < (row + 1) * tree.root_w as usize * 4 {
                        continue;
                    }
                    let s = (row * tree.root_w as usize + rx) * 4;
                    let d = row * stride + rx * 4;
                    copia_ou_mistura(&mut pixels[d..d + rw * 4], &tree.root[s..s + rw * 4], tree.root_alpha);
                }
            } else if let Some(l) = layers.get(&it.key) {
                desenha_layer(l, (it.x - origin.0, it.y - origin.1, it.w as i32, it.h as i32), area, stride, pixels);
            }
        }
        for row in ry..ry + rh {
            for px in pixels[row * stride + rx * 4..row * stride + (rx + rw) * 4].chunks_exact_mut(4) {
                px[3] = 0xFF;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn recompoe<K: Eq + Hash + Clone>(
    tree: &mut Tree<K>,
    novo: Vec<Item<K>>,
    layers: &HashMap<K, Layer>,
    dirty: &mut Dirty<K>,
    origin: (i32, i32),
    dims: (u32, u32),
    pixels: &mut [u8],
    extra: Vec<Rect>,
    tudo: bool,
) -> Vec<Rect> {
    let (ww, wh) = dims;
    let janela = (0, 0, ww as i32, wh as i32);
    let mut tudo = tudo || dirty.all;
    dirty.all = false;
    let mut ret: Vec<Ret> = extra.iter().map(|r| (r.x as i32, r.y as i32, r.w as i32, r.h as i32)).collect();
    let para_janela = |l: Ret| (l.0 - origin.0, l.1 - origin.1, l.2, l.3);
    match compara(&tree.sig, &novo) {
        Mudanca::Igual => {}
        Mudanca::Tudo => tudo = true,
        Mudanca::Areas(a) => ret.extend(a.into_iter().map(para_janela)),
    }
    for (key, partes) in dirty.subs.drain(..) {
        let Some(it) = novo.iter().find(|i| !i.root && i.key == key) else { continue };
        match partes {
            None => ret.push(para_janela(it.ret())),
            Some(rs) => ret.extend(rs.iter().map(|r| para_janela((it.x + r.x as i32, it.y + r.y as i32, r.w as i32, r.h as i32)))),
        }
    }
    tree.sig = novo;
    let rects: Vec<Rect> = if tudo {
        if ww == 0 || wh == 0 {
            Vec::new()
        } else {
            vec![Rect::new(0, 0, ww, wh)]
        }
    } else {
        ret.into_iter().filter_map(|r| intersecta(r, janela)).map(|(x, y, w, h)| Rect::new(x as u32, y as u32, w as u32, h as u32)).collect()
    };
    compoe(tree, layers, origin, dims, pixels, &rects);
    rects
}

pub(crate) fn estica(src: &Layer, (lw, lh): (u32, u32), crop: Ret, dst: &mut [u8]) {
    let (cx, cy, cw, ch) = crop;
    if cw <= 0 || ch <= 0 || lw == 0 || lh == 0 || src.w == 0 || src.h == 0 || dst.len() < cw as usize * ch as usize * 4 || src.px.len() != src.w as usize * src.h as usize * 4 {
        return;
    }
    let (sw, sh) = (src.w as usize, src.h as usize);
    let xs: Vec<usize> = (0..cw as usize).map(|x| ((cx as usize + x) * sw / lw as usize).min(sw - 1)).collect();
    let linha = cw as usize * 4;
    let mut ultima: Option<usize> = None;
    for y in 0..ch as usize {
        let sy = ((cy as usize + y) * sh / lh as usize).min(sh - 1);
        let d = y * linha;
        if y > 0 && ultima == Some(sy) {
            dst.copy_within(d - linha..d, d);
            continue;
        }
        for (x, &sx) in xs.iter().enumerate() {
            dst[d + x * 4..d + x * 4 + 4].copy_from_slice(&src.px[(sy * sw + sx) * 4..(sy * sw + sx) * 4 + 4]);
        }
        ultima = Some(sy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AZUL: [u8; 4] = [0xFF, 0, 0, 0xFF];
    const VERMELHO: [u8; 4] = [0, 0, 0xFF, 0xFF];
    const VERDE: [u8; 4] = [0, 0xFF, 0, 0xFF];

    fn cor(w: u32, h: u32, c: [u8; 4], alpha: bool) -> Layer {
        Layer { w, h, px: c.iter().copied().cycle().take((w * h * 4) as usize).collect(), alpha, lw: w, lh: h }
    }

    fn raiz(w: u32, h: u32, c: [u8; 4]) -> Tree<u32> {
        let mut t = Tree::de_janela(&cor(w, h, c, false).px, w, h, false, (w, h));
        t.sig = vec![Item { key: 0, root: true, x: 0, y: 0, w, h }];
        t
    }

    fn px(p: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    }

    fn item(key: u32, x: i32, y: i32, w: u32, h: u32) -> Item<u32> {
        Item { key, root: false, x, y, w, h }
    }

    #[test]
    fn subsuperficie_opaca_cobre_a_raiz_na_posicao_dela() {
        let mut t = raiz(20, 10, AZUL);
        t.sig.push(item(1, 5, 3, 4, 2));
        let layers = HashMap::from([(1, cor(4, 2, VERMELHO, false))]);
        let mut p = vec![0xA5; 20 * 10 * 4];
        compoe(&t, &layers, (0, 0), (20, 10), &mut p, &[Rect::new(0, 0, 20, 10)]);
        assert_eq!(px(&p, 20, 4, 3), AZUL);
        assert_eq!(px(&p, 20, 5, 3), VERMELHO);
        assert_eq!(px(&p, 20, 8, 4), VERMELHO);
        assert_eq!(px(&p, 20, 9, 4), AZUL);
        assert_eq!(px(&p, 20, 5, 5), AZUL);
    }

    #[test]
    fn a_ordem_do_arranjo_decide_quem_fica_por_cima() {
        let mut t = raiz(10, 10, AZUL);
        t.sig.push(item(1, 0, 0, 6, 6));
        t.sig.push(item(2, 3, 3, 6, 6));
        let layers = HashMap::from([(1, cor(6, 6, VERMELHO, false)), (2, cor(6, 6, VERDE, false))]);
        let mut p = vec![0; 400];
        compoe(&t, &layers, (0, 0), (10, 10), &mut p, &[Rect::new(0, 0, 10, 10)]);
        assert_eq!(px(&p, 10, 4, 4), VERDE);
        t.sig.swap(1, 2);
        compoe(&t, &layers, (0, 0), (10, 10), &mut p, &[Rect::new(0, 0, 10, 10)]);
        assert_eq!(px(&p, 10, 4, 4), VERMELHO);
        assert_eq!(px(&p, 10, 8, 8), VERDE);
    }

    #[test]
    fn so_o_retangulo_pedido_e_tocado() {
        let t = raiz(10, 10, AZUL);
        let mut p = vec![0xA5; 400];
        compoe(&t, &HashMap::new(), (0, 0), (10, 10), &mut p, &[Rect::new(2, 2, 3, 3)]);
        assert_eq!(px(&p, 10, 2, 2), AZUL);
        assert_eq!(px(&p, 10, 4, 4), AZUL);
        assert_eq!(px(&p, 10, 1, 1), [0xA5; 4]);
        assert_eq!(px(&p, 10, 5, 5), [0xA5; 4]);
    }

    #[test]
    fn alfa_pre_multiplicado_mistura_sobre_o_que_esta_embaixo() {
        let mut t = raiz(4, 1, [0, 0, 200, 0xFF]);
        t.sig.push(item(1, 0, 0, 4, 1));
        let layers = HashMap::from([(1, cor(4, 1, [0, 0, 100, 128], true))]);
        let mut p = vec![0; 16];
        compoe(&t, &layers, (0, 0), (4, 1), &mut p, &[Rect::new(0, 0, 4, 1)]);
        let r = px(&p, 4, 0, 0);
        assert_eq!(r[3], 0xFF);
        assert!((i32::from(r[2]) - 200).abs() <= 1, "{r:?}");
        let layers = HashMap::from([(1, cor(4, 1, [0, 0, 0, 0], true))]);
        compoe(&t, &layers, (0, 0), (4, 1), &mut p, &[Rect::new(0, 0, 4, 1)]);
        assert_eq!(px(&p, 4, 1, 0), [0, 0, 200, 0xFF]);
        let layers = HashMap::from([(1, cor(4, 1, VERDE, true))]);
        compoe(&t, &layers, (0, 0), (4, 1), &mut p, &[Rect::new(0, 0, 4, 1)]);
        assert_eq!(px(&p, 4, 2, 0), VERDE);
    }

    #[test]
    fn subsuperficie_com_viewport_e_esticada_por_vizinho_mais_proximo() {
        let mut t = raiz(6, 4, AZUL);
        let mut l = cor(2, 1, VERMELHO, false);
        l.px[4..8].copy_from_slice(&VERDE);
        l.lw = 6;
        l.lh = 4;
        t.sig.push(item(1, 0, 0, 6, 4));
        let layers = HashMap::from([(1, l)]);
        let mut p = vec![0; 6 * 4 * 4];
        compoe(&t, &layers, (0, 0), (6, 4), &mut p, &[Rect::new(0, 0, 6, 4)]);
        assert_eq!(px(&p, 6, 0, 0), VERMELHO);
        assert_eq!(px(&p, 6, 2, 3), VERMELHO);
        assert_eq!(px(&p, 6, 3, 0), VERDE);
        assert_eq!(px(&p, 6, 5, 3), VERDE);
    }

    #[test]
    fn recorte_da_janela_desloca_as_subsuperficies() {
        let mut t = Tree::<u32>::de_janela(&cor(4, 2, AZUL, false).px, 4, 2, false, (30, 40));
        t.sig = vec![Item { key: 0, root: true, x: 0, y: 0, w: 30, h: 40 }, item(1, 11, 20, 2, 1)];
        let layers = HashMap::from([(1, cor(2, 1, VERMELHO, false))]);
        let mut p = vec![0; 32];
        compoe(&t, &layers, (10, 20), (4, 2), &mut p, &[Rect::new(0, 0, 4, 2)]);
        assert_eq!(px(&p, 4, 0, 0), AZUL);
        assert_eq!(px(&p, 4, 1, 0), VERMELHO);
        assert_eq!(px(&p, 4, 2, 0), VERMELHO);
        assert_eq!(px(&p, 4, 3, 0), AZUL);
        assert_eq!(px(&p, 4, 1, 1), AZUL);
    }

    #[test]
    fn compara_arranjos() {
        let a = [Item { key: 0, root: true, x: 0, y: 0, w: 10, h: 10 }, item(1, 1, 1, 2, 2)];
        assert_eq!(compara(&a, &a), Mudanca::Igual);
        let b = [a[0].clone(), a[1].clone(), item(2, 5, 5, 3, 3)];
        assert_eq!(compara(&a, &b), Mudanca::Areas(vec![(5, 5, 3, 3)]));
        assert_eq!(compara(&b, &a), Mudanca::Areas(vec![(5, 5, 3, 3)]));
        let c = [a[0].clone(), item(1, 4, 4, 2, 2)];
        assert_eq!(compara(&a, &c), Mudanca::Areas(vec![(1, 1, 2, 2), (4, 4, 2, 2)]));
        let d = [a[0].clone(), item(2, 5, 5, 3, 3), a[1].clone()];
        assert_eq!(compara(&b, &d), Mudanca::Tudo);
    }

    #[test]
    fn recompoe_so_o_dano_da_subsuperficie_e_limpa_o_que_estava_pendente() {
        let mut t = raiz(20, 10, AZUL);
        let mut layers = HashMap::from([(1, cor(6, 4, VERMELHO, false))]);
        let arranjo = vec![t.sig[0].clone(), item(1, 10, 2, 6, 4)];
        let mut dirty = Dirty::<u32>::default();
        let mut p = vec![0xA5; 20 * 10 * 4];
        let r = recompoe(&mut t, arranjo.clone(), &layers, &mut dirty, (0, 0), (20, 10), &mut p, vec![], false);
        assert_eq!(r, vec![Rect::new(10, 2, 6, 4)]);
        assert_eq!(px(&p, 20, 10, 2), VERMELHO);
        assert_eq!(px(&p, 20, 0, 0), [0xA5; 4]);
        layers.get_mut(&1).unwrap().px = cor(6, 4, VERDE, false).px;
        dirty.subs.push((1, Some(vec![Rect::new(2, 1, 2, 2)])));
        let r = recompoe(&mut t, arranjo, &layers, &mut dirty, (0, 0), (20, 10), &mut p, vec![], false);
        assert_eq!(r, vec![Rect::new(12, 3, 2, 2)]);
        assert!(dirty.subs.is_empty());
        assert_eq!(px(&p, 20, 12, 3), VERDE);
        assert_eq!(px(&p, 20, 10, 2), VERMELHO, "fora do dano fica como estava");
    }

    #[test]
    fn estica_1x1_e_2x1() {
        let um = cor(1, 1, VERMELHO, false);
        let mut d = vec![0; 3 * 2 * 4];
        estica(&um, (3, 2), (0, 0, 3, 2), &mut d);
        assert!(d.chunks_exact(4).all(|c| c == VERMELHO));
        let mut dois = cor(2, 1, VERMELHO, false);
        dois.px[4..8].copy_from_slice(&AZUL);
        let mut d = vec![0; 4 * 2 * 4];
        estica(&dois, (4, 2), (0, 0, 4, 2), &mut d);
        assert_eq!(px(&d, 4, 1, 1), VERMELHO);
        assert_eq!(px(&d, 4, 2, 0), AZUL);
        let mut d = vec![0; 2 * 2 * 4];
        estica(&dois, (4, 2), (2, 0, 2, 2), &mut d);
        assert!(d.chunks_exact(4).all(|c| c == AZUL));
    }
}
