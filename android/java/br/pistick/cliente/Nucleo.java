package br.pistick.cliente;

/**
 * Fronteira com o núcleo em Rust (libps_android.so). De mão única: a casca chama o núcleo; o núcleo não chama Java
 * (docs/26, decisão 17). Os nomes e as assinaturas são o contrato com crates/ps-android/src/jni.rs, onde cada função
 * está documentada; o formato do que {@link #poll} devolve está em crates/ps-android/src/eventos.rs.
 */
final class Nucleo {
    private Nucleo() {
    }

    static {
        System.loadLibrary("ps_android");
    }

    /** Texto de versão do núcleo. */
    static native String versao();

    static native void iniciar(String pasta, String nome);

    /** Liga as linhas VIDEOSTATS e AUDIOSTATS no logcat (extra de depuração). */
    static native void estatisticas(boolean ligado);

    /** Extra de teste: não anuncia o H.264 (a tela segue pelo codec de blocos). */
    static native void semVideo(boolean ligado);

    static native void buscar();

    /** {@code chave}: os 32 bytes da chave do cliente, ou null para o núcleo criar uma (evento ChaveNova). */
    static native void conectar(String endereco, byte[] chave);

    static native void chaveGravada(boolean ok);

    static native void desconectar();

    /** Separa o arquivo PSAKS001 em nonce (12 bytes) + bloco cifrado; null se inválido. */
    static native byte[] chaveDecompor(byte[] arquivo);

    /** Monta o arquivo PSAKS001 a partir de nonce (12 bytes) + bloco cifrado; null se inválido. */
    static native byte[] chaveComporArquivo(byte[] nonceEBloco);

    static native void superficie(android.view.Surface s);

    static native void tela(int largura, int altura, int dpi);

    /** Fator de escala em centésimos (250 = 2,5; de 100 a 400, passo 50); o núcleo valida e limita. */
    static native void escala(int centesimos);

    /** MotionEvent cru: ação mascarada, índice da ação, [id, x, y] por dedo e o instante do evento. */
    static native void toque(int acao, int indice, float[] dados, long tempoMs);

    /** Teclado físico: scanCode do Linux (já é o código evdev). */
    static native void tecla(int scanCode, boolean apertada);

    /** Tecla sem scanCode (adb input, alguns teclados externos): true se o keyCode tem tecla evdev e já foi enviada. */
    static native boolean teclaAndroid(int keyCode, boolean apertada);

    /** Ctrl e Alt grudentos agora, em bits: 1 Ctrl, 2 Alt. */
    static native int modificadores();

    static native void texto(String texto);

    static native void apagar(int antes, int depois);

    /** Tecla da faixa pelo código evdev. */
    static native void teclaExtra(int codigo);

    /** Liga/desliga Ctrl (0) ou Alt (1) grudento; devolve o estado novo. */
    static native boolean modificador(int qual);

    static native void aba(int id);

    static native void focoApp(boolean emFoco, String textoDaArea);

    /** Esvazia a fila de eventos; {@code agoraMs} é SystemClock.uptimeMillis(). */
    static native byte[] poll(long agoraMs);
}
