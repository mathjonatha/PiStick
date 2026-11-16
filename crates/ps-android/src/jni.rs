
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use jni_sys::{jboolean, jbyteArray, jclass, jfloatArray, jint, jlong, jobject, jsize, jstring, JNIEnv, JavaVM, JNI_VERSION_1_6};

use crate::eventos::Segredo;
use crate::gestos::decodificar_toque;
use crate::layout::Tela as TelaInfo;
use crate::registro;
use crate::sessao::{Config, Controle};
use crate::surface::SuperficieAndroid;
use crate::teclado::{evdev_de_android, Mod};
use crate::versao;

static CONTROLE: OnceLock<Mutex<Option<Controle>>> = OnceLock::new();

fn controle() -> Option<Controle> {
    CONTROLE.get().and_then(|m| m.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

fn protegido<T>(neutro: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            registro::erro("pânico no núcleo (interceptado na fronteira JNI)");
            neutro
        }
    }
}

unsafe fn ler_texto(env: *mut JNIEnv, s: jstring) -> Option<String> {
    if s.is_null() {
        return None;
    }
    unsafe {
        let get = (**env).GetStringUTFChars?;
        let rel = (**env).ReleaseStringUTFChars?;
        let p = get(env, s, std::ptr::null_mut());
        if p.is_null() {
            return None;
        }
        let t = CStr::from_ptr(p).to_string_lossy().into_owned();
        rel(env, s, p);
        Some(t)
    }
}

unsafe fn ler_bytes(env: *mut JNIEnv, a: jbyteArray) -> Option<Vec<u8>> {
    if a.is_null() {
        return None;
    }
    unsafe {
        let n = (**env).GetArrayLength?(env, a);
        if n < 0 {
            return None;
        }
        let mut v = vec![0u8; n as usize];
        (**env).GetByteArrayRegion?(env, a, 0, n as jsize, v.as_mut_ptr() as *mut i8);
        Some(v)
    }
}

unsafe fn ler_floats(env: *mut JNIEnv, a: jfloatArray) -> Option<Vec<f32>> {
    if a.is_null() {
        return None;
    }
    unsafe {
        let n = (**env).GetArrayLength?(env, a);
        if n < 0 {
            return None;
        }
        let mut v = vec![0f32; n as usize];
        (**env).GetFloatArrayRegion?(env, a, 0, n as jsize, v.as_mut_ptr());
        Some(v)
    }
}

unsafe fn novo_byte_array(env: *mut JNIEnv, dados: &[u8]) -> jbyteArray {
    unsafe {
        let Some(novo) = (**env).NewByteArray else { return std::ptr::null_mut() };
        let Some(copiar) = (**env).SetByteArrayRegion else { return std::ptr::null_mut() };
        let a = novo(env, dados.len() as jsize);
        if !a.is_null() {
            copiar(env, a, 0, dados.len() as jsize, dados.as_ptr() as *const i8);
        }
        a
    }
}

#[no_mangle]
pub unsafe extern "system" fn JNI_OnLoad(_vm: *mut JavaVM, _reservado: *mut c_void) -> jint {
    registro::info(&format!("núcleo carregado: {}", versao::texto()));
    JNI_VERSION_1_6
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_versao(env: *mut JNIEnv, _classe: jclass) -> jstring {
    let texto = versao::texto();
    let c = CString::new(texto).unwrap_or_default();
    unsafe {
        match (**env).NewStringUTF {
            Some(nova_string) => nova_string(env, c.as_ptr() as *const c_char),
            None => std::ptr::null_mut(),
        }
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_iniciar(env: *mut JNIEnv, _c: jclass, pasta: jstring, nome: jstring) {
    protegido((), || {
        let (pasta, nome) = unsafe { (ler_texto(env, pasta), ler_texto(env, nome)) };
        let (Some(pasta), Some(nome)) = (pasta, nome) else { return };
        let ctl = Controle::new(Config { pasta: PathBuf::from(pasta), nome_cliente: nome });
        ps_client::video::register_decoder_factory(crate::mediacodec::abrir);
        ctl.definir_fabrica_de_saida(|engine| crate::aaudio::SaidaAAudio::abrir(engine).map(|s| Box::new(s) as Box<dyn ps_client::audio::AudioSink>));
        let m = CONTROLE.get_or_init(|| Mutex::new(None));
        if let Some(velho) = m.lock().unwrap_or_else(|e| e.into_inner()).replace(ctl) {
            velho.desconectar();
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_buscar(_env: *mut JNIEnv, _c: jclass) {
    protegido((), || {
        if let Some(c) = controle() {
            c.buscar();
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_estatisticas(_env: *mut JNIEnv, _c: jclass, ligado: jboolean) {
    protegido((), || {
        if let Some(c) = controle() {
            c.estatisticas(ligado != 0);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_semVideo(_env: *mut JNIEnv, _c: jclass, ligado: jboolean) {
    protegido((), || {
        if let Some(c) = controle() {
            c.sem_video(ligado != 0);
        }
    });
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_conectar(env: *mut JNIEnv, _c: jclass, endereco: jstring, chave: jbyteArray) {
    protegido((), || {
        let (endereco, bytes) = unsafe { (ler_texto(env, endereco), ler_bytes(env, chave)) };
        let Some(endereco) = endereco else { return };
        let mut k = None;
        if let Some(mut b) = bytes {
            if b.len() == 32 {
                let mut caixa = Box::new(Segredo([0u8; 32]));
                caixa.0.copy_from_slice(&b);
                k = Some(caixa);
            }
            ps_secure::wipe(&mut b);
        }
        if let Some(c) = controle() {
            c.conectar(&endereco, k);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_chaveGravada(_env: *mut JNIEnv, _c: jclass, ok: jboolean) {
    protegido((), || {
        if let Some(c) = controle() {
            c.chave_gravada(ok != 0);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_desconectar(_env: *mut JNIEnv, _c: jclass) {
    protegido((), || {
        if let Some(c) = controle() {
            c.desconectar();
        }
    });
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_chaveDecompor(env: *mut JNIEnv, _c: jclass, arquivo: jbyteArray) -> jbyteArray {
    protegido(std::ptr::null_mut(), || {
        let Some(arq) = (unsafe { ler_bytes(env, arquivo) }) else { return std::ptr::null_mut() };
        match ps_client::keystore::aks_parse(&arq) {
            Ok((nonce, bloco)) => unsafe { novo_byte_array(env, &[nonce, bloco].concat()) },
            Err(e) => {
                registro::erro(&e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_chaveComporArquivo(env: *mut JNIEnv, _c: jclass, nonce_e_bloco: jbyteArray) -> jbyteArray {
    protegido(std::ptr::null_mut(), || {
        let Some(v) = (unsafe { ler_bytes(env, nonce_e_bloco) }) else { return std::ptr::null_mut() };
        let n = ps_client::keystore::AKS_NONCE_LEN;
        if v.len() <= n {
            return std::ptr::null_mut();
        }
        match ps_client::keystore::aks_compose(&v[..n], &v[n..]) {
            Ok(f) => unsafe { novo_byte_array(env, &f) },
            Err(e) => {
                registro::erro(&e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_superficie(env: *mut JNIEnv, _c: jclass, s: jobject) {
    protegido((), || {
        let Some(ctl) = controle() else { return };
        if s.is_null() {
            ctl.definir_superficie(None);
            return;
        }
        let tela = unsafe { SuperficieAndroid::de_ponteiro(crate::ndk::ANativeWindow_fromSurface(env as *mut c_void, s as *mut c_void)) };
        ctl.definir_superficie(tela.map(|t| Box::new(t) as Box<dyn crate::tela::Tela>));
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_escala(_env: *mut JNIEnv, _c: jclass, centesimos: jint) {
    protegido((), || {
        if let Some(c) = controle() {
            c.escala(centesimos);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_tela(_env: *mut JNIEnv, _c: jclass, largura: jint, altura: jint, dpi: jint) {
    protegido((), || {
        if largura <= 0 || altura <= 0 || largura > 16384 || altura > 16384 {
            return;
        }
        if let Some(c) = controle() {
            c.definir_tela(TelaInfo { largura: largura as u32, altura: altura as u32, dpi: dpi.clamp(0, 2000) as u32 });
        }
    });
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_toque(env: *mut JNIEnv, _c: jclass, acao: jint, indice: jint, dados: jfloatArray, tempo_ms: jlong) {
    protegido((), || {
        let Some(d) = (unsafe { ler_floats(env, dados) }) else { return };
        if let (Some(t), Some(c)) = (decodificar_toque(acao, indice, &d, tempo_ms), controle()) {
            c.toque(&t);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_tecla(_env: *mut JNIEnv, _c: jclass, scan_code: jint, apertada: jboolean) {
    protegido((), || {
        if let Some(c) = controle() {
            c.tecla_fisica(scan_code, apertada != 0);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_teclaAndroid(_env: *mut JNIEnv, _c: jclass, key_code: jint, apertada: jboolean) -> jboolean {
    protegido(0, || {
        let Some(code) = evdev_de_android(key_code) else { return 0 };
        if let Some(c) = controle() {
            c.tecla_fisica(code as i32, apertada != 0);
        }
        1
    })
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_modificadores(_env: *mut JNIEnv, _c: jclass) -> jint {
    protegido(0, || {
        let (ctrl, alt) = controle().map_or((false, false), |c| c.modificadores());
        ctrl as jint | ((alt as jint) << 1)
    })
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_texto(env: *mut JNIEnv, _c: jclass, t: jstring) {
    protegido((), || {
        if let (Some(t), Some(c)) = (unsafe { ler_texto(env, t) }, controle()) {
            c.texto(&t);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_apagar(_env: *mut JNIEnv, _c: jclass, antes: jint, depois: jint) {
    protegido((), || {
        if let Some(c) = controle() {
            c.apagar(antes.max(0) as u32, depois.max(0) as u32);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_teclaExtra(_env: *mut JNIEnv, _c: jclass, codigo: jint) {
    protegido((), || {
        if let Some(c) = controle() {
            c.tecla_extra(codigo.max(0) as u32);
        }
    });
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_modificador(_env: *mut JNIEnv, _c: jclass, qual: jint) -> jboolean {
    protegido(0, || {
        let m = if qual == 1 { Mod::Alt } else { Mod::Ctrl };
        controle().is_some_and(|c| c.alternar_modificador(m)) as jboolean
    })
}

#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_aba(_env: *mut JNIEnv, _c: jclass, id: jint) {
    protegido((), || {
        if let (true, Some(c)) = (id >= 0, controle()) {
            c.focar_aba(id as u32);
        }
    });
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_focoApp(env: *mut JNIEnv, _c: jclass, em_foco: jboolean, texto: jstring) {
    protegido((), || {
        let t = unsafe { ler_texto(env, texto) };
        if let Some(c) = controle() {
            c.foco_do_app(em_foco != 0, t.as_deref());
        }
    });
}

#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_poll(env: *mut JNIEnv, _c: jclass, agora_ms: jlong) -> jbyteArray {
    protegido(std::ptr::null_mut(), || {
        let mut bytes = controle().map(|c| c.poll(agora_ms.max(0) as u64)).unwrap_or_default();
        let a = unsafe { novo_byte_array(env, &bytes) };
        ps_secure::wipe(&mut bytes);
        a
    })
}
