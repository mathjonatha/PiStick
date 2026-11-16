
use std::fs;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::manifesto::{caminho_valido, Entrada, Manifest};
use crate::Erro;

fn erro_io(c: &Path, e: std::io::Error) -> Erro {
    Erro::Io(format!("{}: {e}", c.display()))
}

pub fn hash_do_arquivo(c: &Path) -> Result<([u8; 32], u64), Erro> {
    let mut f = fs::File::open(c).map_err(|e| erro_io(c, e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| erro_io(c, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((h.finalize().into(), total))
}

fn listar(raiz: &Path) -> Result<Vec<(String, std::path::PathBuf)>, Erro> {
    let mut saida = Vec::new();
    let mut pilha = vec![(raiz.to_path_buf(), String::new())];
    while let Some((dir, prefixo)) = pilha.pop() {
        for e in fs::read_dir(&dir).map_err(|e| erro_io(&dir, e))? {
            let e = e.map_err(|e| erro_io(&dir, e))?;
            let nome = e.file_name().into_string().map_err(|n| Erro::Conteudo(format!("nome que não é UTF-8 em {}: {n:?}", dir.display())))?;
            let rel = if prefixo.is_empty() { nome.clone() } else { format!("{prefixo}/{nome}") };
            let tipo = e.file_type().map_err(|e| erro_io(&dir.join(&nome), e))?;
            if tipo.is_symlink() {
                return Err(Erro::Conteudo(format!("link simbólico na pasta: {rel:?} (não é permitido)")));
            } else if tipo.is_dir() {
                pilha.push((dir.join(&nome), rel));
            } else if tipo.is_file() {
                saida.push((rel, dir.join(&nome)));
            } else {
                return Err(Erro::Conteudo(format!("{rel:?} não é arquivo comum nem pasta")));
            }
        }
    }
    saida.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(saida)
}

pub fn montar(pasta: &Path, serial: u64, versao: &str) -> Result<Manifest, Erro> {
    let mut arquivos = Vec::new();
    for (rel, caminho) in listar(pasta)? {
        caminho_valido(&rel).map_err(Erro::Manifesto)?;
        let (sha256, tamanho) = hash_do_arquivo(&caminho)?;
        arquivos.push(Entrada { sha256, tamanho, caminho: rel });
    }
    let m = Manifest { serial, versao: versao.to_string(), arquivos };
    m.validar()?;
    Ok(m)
}

pub fn conferir(pasta: &Path, m: &Manifest) -> Result<usize, Erro> {
    let existentes = listar(pasta)?;
    let mut i = 0;
    for (rel, caminho) in &existentes {
        match m.arquivos.get(i) {
            Some(e) if e.caminho == *rel => {
                let (h, t) = hash_do_arquivo(caminho)?;
                if t != e.tamanho {
                    return Err(Erro::Conteudo(format!("{rel:?} tem {t} bytes, o manifesto diz {}", e.tamanho)));
                }
                if h != e.sha256 {
                    return Err(Erro::Conteudo(format!("{rel:?} tem outro conteúdo (o SHA-256 não confere)")));
                }
                i += 1;
            }
            Some(e) if e.caminho.as_bytes() < rel.as_bytes() => {
                return Err(Erro::Conteudo(format!("falta o arquivo {:?}", e.caminho)));
            }
            _ => return Err(Erro::Conteudo(format!("arquivo a mais na pasta, que o manifesto não lista: {rel:?}"))),
        }
    }
    if let Some(e) = m.arquivos.get(i) {
        return Err(Erro::Conteudo(format!("falta o arquivo {:?}", e.caminho)));
    }
    Ok(i)
}
