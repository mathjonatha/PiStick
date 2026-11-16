
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key as AeadKey, Nonce, Tag};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

pub const PROTOCOL_NAME: &[u8] = b"Noise_XX_25519_ChaChaPoly_SHA256";
pub const HASH_LEN: usize = 32;
pub const KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const MAX_MESSAGE: usize = 65535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseError {
    Decrypt,
    BadLength,
    BadState,
    WeakKey,
    NonceExhausted,
    Random,
}

impl std::fmt::Display for NoiseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NoiseError::Decrypt => "falha de autenticação do recado",
            NoiseError::BadLength => "tamanho de recado inválido",
            NoiseError::BadState => "recado fora de ordem ou aperto de mão já falhou",
            NoiseError::WeakKey => "chave pública de ordem baixa recusada",
            NoiseError::NonceExhausted => "contador de nonce esgotado",
            NoiseError::Random => "falha no gerador de números aleatórios",
        })
    }
}

impl std::error::Error for NoiseError {}

#[derive(Clone)]
pub struct Secret([u8; 32]);

impl Secret {
    pub fn new(b: [u8; 32]) -> Self {
        Secret(b)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Clone)]
pub struct Keypair {
    secret: Secret,
    public: [u8; 32],
}

impl Keypair {
    pub fn from_secret(secret: [u8; 32]) -> Keypair {
        let public = x25519_dalek::x25519(secret, x25519_dalek::X25519_BASEPOINT_BYTES);
        Keypair { secret: Secret(secret), public }
    }

    pub fn generate() -> Result<Keypair, NoiseError> {
        let mut s = [0u8; 32];
        getrandom::fill(&mut s).map_err(|_| NoiseError::Random)?;
        let kp = Keypair::from_secret(s);
        s.zeroize();
        Ok(kp)
    }

    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn secret_bytes(&self) -> &[u8; 32] {
        self.secret.as_bytes()
    }

    fn dh(&self, public: &[u8; 32]) -> Result<Secret, NoiseError> {
        let mut out = x25519_dalek::x25519(*self.secret.as_bytes(), *public);
        let fraca = crate::ct_eq(&out, &[0u8; 32]);
        let secret = Secret(out);
        out.zeroize();
        if fraca {
            return Err(NoiseError::WeakKey);
        }
        Ok(secret)
    }
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keypair(..)")
    }
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..32 {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for p in parts {
        inner.update(p);
    }
    let inner: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let out = outer.finalize().into();
    ipad.zeroize();
    opad.zeroize();
    out
}

fn hkdf2(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut temp = hmac(ck, &[ikm]);
    let out1 = hmac(&temp, &[&[0x01]]);
    let out2 = hmac(&temp, &[&out1, &[0x02]]);
    temp.zeroize();
    (out1, out2)
}

#[doc(hidden)]
pub fn hmac_sha256_for_test(key: &[u8; 32], data: &[u8]) -> [u8; 32] {
    hmac(key, &[data])
}

pub struct CipherState {
    k: Option<Secret>,
    n: u64,
}

impl CipherState {
    fn empty() -> Self {
        CipherState { k: None, n: 0 }
    }

    fn with_key(k: [u8; 32]) -> Self {
        CipherState { k: Some(Secret(k)), n: 0 }
    }

    pub fn has_key(&self) -> bool {
        self.k.is_some()
    }

    pub fn nonce(&self) -> u64 {
        self.n
    }

    fn nonce_bytes(n: u64) -> Nonce {
        let mut b = [0u8; 12];
        b[4..].copy_from_slice(&n.to_le_bytes());
        b.into()
    }

    pub fn encrypt_detached(&mut self, ad: &[u8], buf: &mut [u8]) -> Result<[u8; TAG_LEN], NoiseError> {
        let Some(k) = &self.k else { return Err(NoiseError::BadState) };
        if self.n == u64::MAX {
            return Err(NoiseError::NonceExhausted);
        }
        let cipher = ChaCha20Poly1305::new(AeadKey::from_slice(k.as_bytes()));
        let tag = cipher.encrypt_in_place_detached(&Self::nonce_bytes(self.n), ad, buf).map_err(|_| NoiseError::Decrypt)?;
        self.n += 1;
        Ok(tag.into())
    }

    pub fn encrypt_in_place(&mut self, ad: &[u8], buf: &mut Vec<u8>) -> Result<(), NoiseError> {
        if self.k.is_none() {
            return Ok(());
        }
        let tag = self.encrypt_detached(ad, buf)?;
        buf.extend_from_slice(&tag);
        Ok(())
    }

    pub fn decrypt_in_place(&mut self, ad: &[u8], buf: &mut Vec<u8>) -> Result<(), NoiseError> {
        let Some(k) = &self.k else { return Ok(()) };
        if self.n == u64::MAX {
            return Err(NoiseError::NonceExhausted);
        }
        if buf.len() < TAG_LEN {
            return Err(NoiseError::BadLength);
        }
        let split = buf.len() - TAG_LEN;
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&buf[split..]);
        buf.truncate(split);
        let cipher = ChaCha20Poly1305::new(AeadKey::from_slice(k.as_bytes()));
        cipher.decrypt_in_place_detached(&Self::nonce_bytes(self.n), ad, buf, Tag::from_slice(&tag)).map_err(|_| NoiseError::Decrypt)?;
        self.n += 1;
        Ok(())
    }

    #[doc(hidden)]
    pub fn set_nonce_for_test(&mut self, n: u64) {
        self.n = n;
    }
}

struct SymmetricState {
    ck: [u8; 32],
    h: [u8; 32],
    cs: CipherState,
}

impl SymmetricState {
    fn new() -> Self {
        let mut h = [0u8; 32];
        h.copy_from_slice(PROTOCOL_NAME);
        SymmetricState { ck: h, h, cs: CipherState::empty() }
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.h = sha256(&[&self.h, data]);
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let (ck, mut temp_k) = hkdf2(&self.ck, ikm);
        self.ck = ck;
        self.cs = CipherState::with_key(temp_k);
        temp_k.zeroize();
    }

    fn encrypt_and_hash(&mut self, plain: &[u8], out: &mut Vec<u8>) -> Result<(), NoiseError> {
        let start = out.len();
        out.extend_from_slice(plain);
        let mut tmp = out.split_off(start);
        let h = self.h;
        self.cs.encrypt_in_place(&h, &mut tmp)?;
        self.mix_hash(&tmp);
        out.extend_from_slice(&tmp);
        Ok(())
    }

    fn decrypt_and_hash(&mut self, cipher: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = cipher.to_vec();
        let h = self.h;
        self.cs.decrypt_in_place(&h, &mut buf)?;
        self.mix_hash(cipher);
        Ok(buf)
    }

    fn split(&self) -> (CipherState, CipherState) {
        let (mut k1, mut k2) = hkdf2(&self.ck, &[]);
        let r = (CipherState::with_key(k1), CipherState::with_key(k2));
        k1.zeroize();
        k2.zeroize();
        r
    }
}

impl Drop for SymmetricState {
    fn drop(&mut self) {
        self.ck.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

pub struct Established {
    pub send: CipherState,
    pub recv: CipherState,
    pub handshake_hash: [u8; 32],
    pub remote_static: [u8; 32],
}

pub struct HandshakeState {
    role: Role,
    ss: SymmetricState,
    s: Keypair,
    e: Option<Keypair>,
    rs: Option<[u8; 32]>,
    re: Option<[u8; 32]>,
    step: u8,
    failed: bool,
    forced_e: Option<[u8; 32]>,
}

impl HandshakeState {
    pub fn new(role: Role, prologue: &[u8], s: Keypair) -> HandshakeState {
        let mut ss = SymmetricState::new();
        ss.mix_hash(prologue);
        HandshakeState { role, ss, s, e: None, rs: None, re: None, step: 0, failed: false, forced_e: None }
    }

    #[doc(hidden)]
    pub fn with_ephemeral_for_test(mut self, e: [u8; 32]) -> Self {
        self.forced_e = Some(e);
        self
    }

    pub fn is_finished(&self) -> bool {
        self.step == 3 && !self.failed
    }

    pub fn my_turn_to_write(&self) -> bool {
        matches!((self.role, self.step), (Role::Initiator, 0 | 2) | (Role::Responder, 1))
    }

    pub fn remote_static(&self) -> Option<[u8; 32]> {
        self.rs
    }

    pub fn handshake_hash(&self) -> [u8; 32] {
        self.ss.h
    }

    fn new_ephemeral(&mut self) -> Result<(), NoiseError> {
        self.e = Some(match self.forced_e.take() {
            Some(e) => Keypair::from_secret(e),
            None => Keypair::generate()?,
        });
        Ok(())
    }

    pub fn write_message(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<(), NoiseError> {
        if self.failed || !self.my_turn_to_write() {
            return Err(NoiseError::BadState);
        }
        let r = self.write_inner(payload, out);
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    fn write_inner(&mut self, payload: &[u8], out: &mut Vec<u8>) -> Result<(), NoiseError> {
        let start = out.len();
        match self.step {
            0 => {
                self.new_ephemeral()?;
                let epub = *self.e.as_ref().ok_or(NoiseError::BadState)?.public();
                out.extend_from_slice(&epub);
                self.ss.mix_hash(&epub);
            }
            1 => {
                self.new_ephemeral()?;
                let e = self.e.as_ref().ok_or(NoiseError::BadState)?;
                let epub = *e.public();
                out.extend_from_slice(&epub);
                self.ss.mix_hash(&epub);
                let re = self.re.ok_or(NoiseError::BadState)?;
                let ee = e.dh(&re)?;
                self.ss.mix_key(ee.as_bytes());
                let spub = *self.s.public();
                self.ss.encrypt_and_hash(&spub, out)?;
                let es = self.s.dh(&re)?;
                self.ss.mix_key(es.as_bytes());
            }
            2 => {
                let spub = *self.s.public();
                self.ss.encrypt_and_hash(&spub, out)?;
                let re = self.re.ok_or(NoiseError::BadState)?;
                let se = self.s.dh(&re)?;
                self.ss.mix_key(se.as_bytes());
            }
            _ => return Err(NoiseError::BadState),
        }
        self.ss.encrypt_and_hash(payload, out)?;
        if out.len() - start > MAX_MESSAGE {
            return Err(NoiseError::BadLength);
        }
        self.step += 1;
        Ok(())
    }

    pub fn read_message(&mut self, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if self.failed || self.my_turn_to_write() || self.step >= 3 {
            return Err(NoiseError::BadState);
        }
        let r = self.read_inner(msg);
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    fn read_inner(&mut self, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if msg.len() > MAX_MESSAGE {
            return Err(NoiseError::BadLength);
        }
        let mut pos = 0usize;
        let mut take = |n: usize| -> Result<&[u8], NoiseError> {
            let s = msg.get(pos..pos + n).ok_or(NoiseError::BadLength)?;
            pos += n;
            Ok(s)
        };
        match self.step {
            0 => {
                let mut re = [0u8; 32];
                re.copy_from_slice(take(32)?);
                self.re = Some(re);
                self.ss.mix_hash(&re);
            }
            1 => {
                let mut re = [0u8; 32];
                re.copy_from_slice(take(32)?);
                self.re = Some(re);
                self.ss.mix_hash(&re);
                let ee = self.e.as_ref().ok_or(NoiseError::BadState)?.dh(&re)?;
                self.ss.mix_key(ee.as_bytes());
                let enc_s = take(KEY_LEN + TAG_LEN)?;
                let rs_bytes = self.ss.decrypt_and_hash(enc_s)?;
                let mut rs = [0u8; 32];
                rs.copy_from_slice(&rs_bytes);
                self.rs = Some(rs);
                let es = self.e.as_ref().ok_or(NoiseError::BadState)?.dh(&rs)?;
                self.ss.mix_key(es.as_bytes());
            }
            2 => {
                let enc_s = take(KEY_LEN + TAG_LEN)?;
                let rs_bytes = self.ss.decrypt_and_hash(enc_s)?;
                let mut rs = [0u8; 32];
                rs.copy_from_slice(&rs_bytes);
                self.rs = Some(rs);
                let se = self.e.as_ref().ok_or(NoiseError::BadState)?.dh(&rs)?;
                self.ss.mix_key(se.as_bytes());
            }
            _ => return Err(NoiseError::BadState),
        }
        let payload = self.ss.decrypt_and_hash(&msg[pos..])?;
        self.step += 1;
        Ok(payload)
    }

    pub fn finish(self) -> Result<Established, NoiseError> {
        if !self.is_finished() {
            return Err(NoiseError::BadState);
        }
        let (c1, c2) = self.ss.split();
        let (send, recv) = match self.role {
            Role::Initiator => (c1, c2),
            Role::Responder => (c2, c1),
        };
        Ok(Established { send, recv, handshake_hash: self.ss.h, remote_static: self.rs.ok_or(NoiseError::BadState)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn hmac_confere_com_os_vetores_da_rfc_4231() {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(b"Jefe");
        assert_eq!(hex(&hmac_sha256_for_test(&key, b"what do ya want for nothing?")), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        let mut key = [0u8; 32];
        key[..20].fill(0xaa);
        assert_eq!(hex(&hmac_sha256_for_test(&key, &[0xddu8; 50])), "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe");
    }

    #[test]
    fn aperto_de_mao_completo_e_transporte() {
        let a = Keypair::generate().unwrap();
        let b = Keypair::generate().unwrap();
        let (apub, bpub) = (*a.public(), *b.public());
        let mut i = HandshakeState::new(Role::Initiator, b"PSN1", a);
        let mut r = HandshakeState::new(Role::Responder, b"PSN1", b);
        let mut m1 = Vec::new();
        i.write_message(b"", &mut m1).unwrap();
        assert_eq!(m1.len(), 32);
        assert_eq!(r.read_message(&m1).unwrap(), b"");
        let mut m2 = Vec::new();
        r.write_message(b"pi", &mut m2).unwrap();
        assert_eq!(m2.len(), 32 + 48 + 2 + 16);
        assert_eq!(i.read_message(&m2).unwrap(), b"pi");
        let mut m3 = Vec::new();
        i.write_message(b"notebook", &mut m3).unwrap();
        assert_eq!(m3.len(), 48 + 8 + 16);
        assert_eq!(r.read_message(&m3).unwrap(), b"notebook");
        assert_eq!(i.remote_static(), Some(bpub));
        assert_eq!(r.remote_static(), Some(apub));
        let (ei, er) = (i.finish().unwrap(), r.finish().unwrap());
        assert_eq!(ei.handshake_hash, er.handshake_hash);
        let (mut ei, mut er) = (ei, er);
        let mut buf = b"ola".to_vec();
        ei.send.encrypt_in_place(b"", &mut buf).unwrap();
        er.recv.decrypt_in_place(b"", &mut buf).unwrap();
        assert_eq!(buf, b"ola");
    }

    #[test]
    fn recado_fora_de_ordem_e_recusado() {
        let mut i = HandshakeState::new(Role::Initiator, b"x", Keypair::generate().unwrap());
        assert_eq!(i.read_message(&[0u8; 32]).unwrap_err(), NoiseError::BadState);
        let mut r = HandshakeState::new(Role::Responder, b"x", Keypair::generate().unwrap());
        let mut out = Vec::new();
        assert_eq!(r.write_message(b"", &mut out).unwrap_err(), NoiseError::BadState);
    }
}
