package br.pistick.cliente;

final class Nucleo {
    private Nucleo() {
    }

    static {
        System.loadLibrary("ps_android");
    }

    static native String versao();

    static native void iniciar(String pasta, String nome);

    static native void estatisticas(boolean ligado);

    static native void semVideo(boolean ligado);

    static native void buscar();

    static native void conectar(String endereco, byte[] chave);

    static native void chaveGravada(boolean ok);

    static native void desconectar();

    static native byte[] chaveDecompor(byte[] arquivo);

    static native byte[] chaveComporArquivo(byte[] nonceEBloco);

    static native void superficie(android.view.Surface s);

    static native void tela(int largura, int altura, int dpi);

    static native void escala(int centesimos);

    static native void toque(int acao, int indice, float[] dados, long tempoMs);

    static native void tecla(int scanCode, boolean apertada);

    static native boolean teclaAndroid(int keyCode, boolean apertada);

    static native int modificadores();

    static native void texto(String texto);

    static native void apagar(int antes, int depois);

    static native void teclaExtra(int codigo);

    static native boolean modificador(int qual);

    static native void aba(int id);

    static native void focoApp(boolean emFoco, String textoDaArea);

    static native byte[] poll(long agoraMs);
}
