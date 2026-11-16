//! Fronteira JNI entre a casca Java (`android/java/br/pistick/cliente/Nucleo.java`) e o núcleo (docs/26, decisão 17):
//! **de mão única**. A casca chama as funções `Java_br_pistick_cliente_Nucleo_*` daqui; o núcleo nunca chama Java
//! (sem `AttachCurrentThread`, sem referências globais, sem retorno de chamada nas threads de rede). O que o núcleo
//! tem a dizer à casca sai por `poll()`, no formato binário documentado em `eventos.rs`.
//!
//! Funções (todas `static native` de `Nucleo`):
//!
//! | função | o que faz |
//! |---|---|
//! | `versao()` | texto de versão |
//! | `iniciar(pasta, nome)` | cria o controle do núcleo; `pasta` é `getFilesDir()` |
//! | `buscar()` | procura aparelhos por mDNS (eventos `Aparelho` e `BuscaFim`) |
//! | `estatisticas(ligado)` | liga as linhas `VIDEOSTATS` e `AUDIOSTATS` no `logcat` (extra de depuração) |
//! | `semVideo(ligado)` | extra de teste: não anuncia `CAP_H264` |
//! | `conectar(endereco, chave)` | conecta; `chave` são os 32 bytes do cliente (ou `null`: o núcleo cria e devolve no evento `ChaveNova`) |
//! | `chaveGravada(ok)` | a casca diz se gravou a chave nova |
//! | `desconectar()` | fecha a conexão |
//! | `chaveDecompor(arquivo)` | separa o arquivo `PSAKS001` em `nonce` (12 bytes) + `bloco`; `null` se inválido |
//! | `chaveComporArquivo(nonceEBloco)` | monta o arquivo `PSAKS001` de `nonce` + `bloco`; `null` se inválido |
//! | `superficie(Surface)` | a superfície de desenho (`null` a solta) |
//! | `tela(largura, altura, dpi)` | tamanho da superfície em pixels e densidade |
//! | `escala(centesimos)` | fator de escala do texto em centésimos (250 = 2,5; passo 0,5, de 1,0 a 4,0, validado no núcleo); refaz o pedido de tamanho das janelas e vale para as próximas conexões |
//! | `toque(acao, indice, dados, tempoMs)` | `MotionEvent` cru; `dados` = `[id, x, y]` por dedo |
//! | `tecla(scanCode, apertada)` | teclado físico |
//! | `teclaAndroid(keyCode, apertada)` | tecla sem `scanCode` (`adb input`, alguns teclados externos) pelo `keyCode`; `true` se mapeada, `false` se a casca deve mandar o caractere por `texto` |
//! | `modificadores()` | Ctrl e Alt grudentos agora, em bits (1 Ctrl, 2 Alt); a casca consulta depois de cada tecla ou texto |
//! | `texto(texto)`, `apagar(antes, depois)` | `InputConnection` do teclado virtual |
//! | `teclaExtra(codigoEvdev)`, `modificador(qual)` | faixa de teclas (`qual`: 0 Ctrl, 1 Alt; devolve o estado novo) |
//! | `aba(id)` | foca uma janela |
//! | `focoApp(emFoco, textoDaArea)` | o aplicativo ganhou/perdeu o foco; com o texto da área de transferência |
//! | `poll(agoraMs)` | esvazia a fila de eventos (e anda o relógio dos gestos); `agoraMs` é `SystemClock.uptimeMillis()` |
//!
//! Nenhuma função entra em pânico para fora: cada uma roda sob `catch_unwind` e devolve o valor neutro se algo der
//! errado. As declarações da tabela do JNI vêm do crate `jni-sys` 0.3.0. `unsafe` só neste arquivo, em `surface.rs` e
//! em `ndk.rs`.

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

/// Roda `f` sem deixar um pânico atravessar a fronteira com a máquina virtual.
fn protegido<T>(neutro: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            registro::erro("pânico no núcleo (interceptado na fronteira JNI)");
            neutro
        }
    }
}

// ---- conversões ------------------------------------------------------------------------------------------------

/// `jstring` → `String` (UTF-8 modificado do Java; pares substitutos viram U+FFFD, o que não afeta nada aqui).
///
/// # Safety
/// `env` é o ambiente da thread atual e `s` é nulo ou uma `jstring` viva.
unsafe fn ler_texto(env: *mut JNIEnv, s: jstring) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `env` válido (contrato); `*env` é a tabela de funções; `GetStringUTFChars` devolve uma string C
    // terminada em NUL (ou nulo) que vale até o `ReleaseStringUTFChars` abaixo; copiamos antes de soltar.
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

/// `jbyteArray` → `Vec<u8>`.
///
/// # Safety
/// `env` é o ambiente da thread atual e `a` é nulo ou um `byte[]` vivo.
unsafe fn ler_bytes(env: *mut JNIEnv, a: jbyteArray) -> Option<Vec<u8>> {
    if a.is_null() {
        return None;
    }
    // SAFETY: `env` válido (contrato); o tamanho vem do próprio array e o destino tem exatamente esse tamanho.
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

/// `jfloatArray` → `Vec<f32>`.
///
/// # Safety
/// `env` é o ambiente da thread atual e `a` é nulo ou um `float[]` vivo.
unsafe fn ler_floats(env: *mut JNIEnv, a: jfloatArray) -> Option<Vec<f32>> {
    if a.is_null() {
        return None;
    }
    // SAFETY: como em `ler_bytes`.
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

/// `&[u8]` → `byte[]` novo (nulo se a máquina virtual não conseguiu alocar).
///
/// # Safety
/// `env` é o ambiente da thread atual.
unsafe fn novo_byte_array(env: *mut JNIEnv, dados: &[u8]) -> jbyteArray {
    // SAFETY: `env` válido (contrato); o array novo tem `dados.len()` bytes e a cópia cobre exatamente isso.
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

// ---- funções chamadas pela casca -------------------------------------------------------------------------------

/// Chamada pelo carregador quando a casca faz `System.loadLibrary("ps_android")`.
///
/// # Safety
/// Só o ambiente de execução do Android a chama, com `vm` e `reservado` dele.
#[no_mangle]
pub unsafe extern "system" fn JNI_OnLoad(_vm: *mut JavaVM, _reservado: *mut c_void) -> jint {
    registro::info(&format!("núcleo carregado: {}", versao::texto()));
    JNI_VERSION_1_6
}

/// `static native String versao()`.
///
/// # Safety
/// Só a máquina virtual a chama, com um `env` válido da thread atual.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_versao(env: *mut JNIEnv, _classe: jclass) -> jstring {
    let texto = versao::texto();
    // O texto vem de `format!` sobre constantes e números: nunca tem NUL; se tivesse, devolve vazio em vez de cair.
    let c = CString::new(texto).unwrap_or_default();
    // SAFETY: `env` é o ponteiro da máquina virtual para a thread atual (válido durante esta chamada); `*env` é a
    // tabela de funções do JNI e `NewStringUTF` é uma entrada dela (`Option` só por causa do tipo de `jni-sys`; a
    // máquina virtual a preenche sempre); `c` é uma string C terminada em NUL que vive até o fim da chamada, e o JNI
    // copia o conteúdo antes de voltar.
    unsafe {
        match (**env).NewStringUTF {
            Some(nova_string) => nova_string(env, c.as_ptr() as *const c_char),
            None => std::ptr::null_mut(),
        }
    }
}

/// `static native void iniciar(String pasta, String nome)`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_iniciar(env: *mut JNIEnv, _c: jclass, pasta: jstring, nome: jstring) {
    protegido((), || {
        // SAFETY: contrato da função.
        let (pasta, nome) = unsafe { (ler_texto(env, pasta), ler_texto(env, nome)) };
        let (Some(pasta), Some(nome)) = (pasta, nome) else { return };
        let ctl = Controle::new(Config { pasta: PathBuf::from(pasta), nome_cliente: nome });
        // O decodificador H.264 do aparelho (AMediaCodec) e a saída de som (AAudio).
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

/// `static native void estatisticas(boolean ligado)`: liga as linhas VIDEOSTATS e AUDIOSTATS no `logcat`.
#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_estatisticas(_env: *mut JNIEnv, _c: jclass, ligado: jboolean) {
    protegido((), || {
        if let Some(c) = controle() {
            c.estatisticas(ligado != 0);
        }
    });
}

/// `static native void semVideo(boolean ligado)`: extra de teste, não anuncia `CAP_H264`.
#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_semVideo(_env: *mut JNIEnv, _c: jclass, ligado: jboolean) {
    protegido((), || {
        if let Some(c) = controle() {
            c.sem_video(ligado != 0);
        }
    });
}

/// `static native void conectar(String endereco, byte[] chave)`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_conectar(env: *mut JNIEnv, _c: jclass, endereco: jstring, chave: jbyteArray) {
    protegido((), || {
        // SAFETY: contrato da função.
        let (endereco, bytes) = unsafe { (ler_texto(env, endereco), ler_bytes(env, chave)) };
        let Some(endereco) = endereco else { return };
        // A chave vai direto para uma caixa que se apaga ao ser solta: nenhuma cópia de 32 bytes na pilha.
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

/// `static native byte[] chaveDecompor(byte[] arquivo)`: `nonce (12) + bloco`, ou `null`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_chaveDecompor(env: *mut JNIEnv, _c: jclass, arquivo: jbyteArray) -> jbyteArray {
    protegido(std::ptr::null_mut(), || {
        // SAFETY: contrato da função.
        let Some(arq) = (unsafe { ler_bytes(env, arquivo) }) else { return std::ptr::null_mut() };
        match ps_client::keystore::aks_parse(&arq) {
            // SAFETY: contrato da função.
            Ok((nonce, bloco)) => unsafe { novo_byte_array(env, &[nonce, bloco].concat()) },
            Err(e) => {
                registro::erro(&e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// `static native byte[] chaveComporArquivo(byte[] nonceEBloco)`: o arquivo `PSAKS001`, ou `null`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_chaveComporArquivo(env: *mut JNIEnv, _c: jclass, nonce_e_bloco: jbyteArray) -> jbyteArray {
    protegido(std::ptr::null_mut(), || {
        // SAFETY: contrato da função.
        let Some(v) = (unsafe { ler_bytes(env, nonce_e_bloco) }) else { return std::ptr::null_mut() };
        let n = ps_client::keystore::AKS_NONCE_LEN;
        if v.len() <= n {
            return std::ptr::null_mut();
        }
        match ps_client::keystore::aks_compose(&v[..n], &v[n..]) {
            // SAFETY: contrato da função.
            Ok(f) => unsafe { novo_byte_array(env, &f) },
            Err(e) => {
                registro::erro(&e.to_string());
                std::ptr::null_mut()
            }
        }
    })
}

/// `static native void superficie(Surface s)`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e `s` nulo ou uma `Surface` viva.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_superficie(env: *mut JNIEnv, _c: jclass, s: jobject) {
    protegido((), || {
        let Some(ctl) = controle() else { return };
        if s.is_null() {
            ctl.definir_superficie(None);
            return;
        }
        // SAFETY: `env` e `s` válidos (contrato); `ANativeWindow_fromSurface` adquire uma referência que passa a
        // pertencer à `SuperficieAndroid` (solta no `Drop`); o ponteiro nulo (falha) vira `None`.
        let tela = unsafe { SuperficieAndroid::de_ponteiro(crate::ndk::ANativeWindow_fromSurface(env as *mut c_void, s as *mut c_void)) };
        ctl.definir_superficie(tela.map(|t| Box::new(t) as Box<dyn crate::tela::Tela>));
    });
}

/// `static native void escala(int centesimos)`: fator de escala em centésimos (250 = 2,5); o núcleo valida e limita.
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

/// `static native void toque(int acao, int indice, float[] dados, long tempoMs)`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_toque(env: *mut JNIEnv, _c: jclass, acao: jint, indice: jint, dados: jfloatArray, tempo_ms: jlong) {
    protegido((), || {
        // SAFETY: contrato da função.
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

/// `static native boolean teclaAndroid(int keyCode, boolean apertada)`: tecla sem `scanCode` pelo `keyCode` do Android.
/// Devolve `true` se o `keyCode` tem tecla evdev (e já foi enviada); `false` se a casca deve tentar o caractere.
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

/// `static native int modificadores()`: Ctrl e Alt grudentos agora, em bits (1 Ctrl, 2 Alt).
#[no_mangle]
pub extern "system" fn Java_br_pistick_cliente_Nucleo_modificadores(_env: *mut JNIEnv, _c: jclass) -> jint {
    protegido(0, || {
        let (ctrl, alt) = controle().map_or((false, false), |c| c.modificadores());
        ctrl as jint | ((alt as jint) << 1)
    })
}

/// `static native void texto(String t)`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_texto(env: *mut JNIEnv, _c: jclass, t: jstring) {
    protegido((), || {
        // SAFETY: contrato da função.
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

/// Alterna Ctrl (0) ou Alt (1); devolve o estado novo.
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

/// `static native void focoApp(boolean emFoco, String textoDaArea)`.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido e argumentos vivos.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_focoApp(env: *mut JNIEnv, _c: jclass, em_foco: jboolean, texto: jstring) {
    protegido((), || {
        // SAFETY: contrato da função.
        let t = unsafe { ler_texto(env, texto) };
        if let Some(c) = controle() {
            c.foco_do_app(em_foco != 0, t.as_deref());
        }
    });
}

/// `static native byte[] poll(long agoraMs)`: os eventos pendentes (vazio se não há); o `byte[]` que sai pode conter a
/// chave nova: a casca o apaga depois de ler, e o núcleo apaga a cópia dele aqui.
///
/// # Safety
/// Chamada pela máquina virtual com `env` válido.
#[no_mangle]
pub unsafe extern "system" fn Java_br_pistick_cliente_Nucleo_poll(env: *mut JNIEnv, _c: jclass, agora_ms: jlong) -> jbyteArray {
    protegido(std::ptr::null_mut(), || {
        let mut bytes = controle().map(|c| c.poll(agora_ms.max(0) as u64)).unwrap_or_default();
        // SAFETY: contrato da função.
        let a = unsafe { novo_byte_array(env, &bytes) };
        ps_secure::wipe(&mut bytes);
        a
    })
}
