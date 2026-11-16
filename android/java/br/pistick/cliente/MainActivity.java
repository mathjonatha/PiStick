package br.pistick.cliente;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.SharedPreferences;
import android.content.pm.ApplicationInfo;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.net.wifi.WifiManager;
import android.os.Bundle;
import android.os.SystemClock;
import android.text.InputType;
import android.util.Log;
import android.view.Choreographer;
import android.view.Gravity;
import android.view.HapticFeedbackConstants;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewGroup;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.HorizontalScrollView;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

public final class MainActivity extends Activity implements Choreographer.FrameCallback, SurfaceHolder.Callback {
    private static final String ETIQUETA = "PiStick";
    private static final String NOME_CLIENTE = "celular";
    private static final String PREFS = "pistick";
    private static final String PREF_ESCALA = "escala_centesimos";
    private static final int ESCALA_PADRAO = 250;
    private static final int ESCALA_MIN = 100;
    private static final int ESCALA_MAX = 400;
    private static final int ESCALA_PASSO = 50;

    private static final int FASE_OCIOSO = 0;
    private static final int FASE_BUSCANDO = 1;
    private static final int FASE_CONECTANDO = 2;
    private static final int FASE_PAREANDO = 3;
    private static final int FASE_CONECTADO = 4;
    private static final int FASE_DESCONECTADO = 5;
    private static final int FASE_ERRO = 6;

    private static final int KEY_ESC = 1;
    private static final int KEY_TAB = 15;
    private static final int KEY_HOME = 102;
    private static final int KEY_UP = 103;
    private static final int KEY_LEFT = 105;
    private static final int KEY_RIGHT = 106;
    private static final int KEY_END = 107;
    private static final int KEY_DOWN = 108;

    private FrameLayout raiz;
    private ScrollView telaLista;
    private LinearLayout telaPareamento;
    private LinearLayout telaSessao;

    private TextView estadoLista;
    private LinearLayout aparelhos;
    private EditText campoEndereco;
    private TextView estadoPareamento;
    private TextView codigoPareamento;
    private TextView avisoSessao;
    private LinearLayout abas;
    private SurfaceView superficie;
    private EntradaView entrada;
    private Button botaoCtrl;
    private Button botaoAlt;
    private Button botaoMenos;
    private Button botaoMais;
    private int escala = ESCALA_PADRAO;

    private WifiManager.MulticastLock travaMulticast;
    private boolean quadrosAtivos;
    private boolean noPrimeiroPlano;
    private boolean emSessao;
    private String ultimoEndereco;
    private boolean reconectar;
    private boolean reconectarAoVoltar;
    private int focoAtual;
    private boolean tecladoAberto;
    private AudioFocusRequest focoDeAudio;

    @Override
    protected void onCreate(Bundle estado) {
        super.onCreate(estado);
        try {
            Log.i(ETIQUETA, "versão do núcleo: " + Nucleo.versao());
        } catch (UnsatisfiedLinkError e) {
            TextView t = new TextView(this);
            t.setText("O núcleo nativo não carregou: " + e.getMessage());
            setContentView(t);
            return;
        }
        Nucleo.iniciar(getFilesDir().getAbsolutePath(), NOME_CLIENTE);
        setVolumeControlStream(AudioManager.STREAM_MUSIC);
        montarTelas();
        SharedPreferences prefs = getSharedPreferences(PREFS, MODE_PRIVATE);
        escala = Math.max(ESCALA_MIN, Math.min(ESCALA_MAX, prefs.getInt(PREF_ESCALA, ESCALA_PADRAO)));
        aplicarEscala(false);
        mostrar(telaLista);
        String direto = getIntent().getStringExtra("endereco");
        boolean depuravel = (getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0;
        if (depuravel) {
            String est = getIntent().getStringExtra("estatisticas");
            if ("1".equals(est) || getIntent().getIntExtra("estatisticas", 0) == 1) {
                Nucleo.estatisticas(true);
            }
            String semVideo = getIntent().getStringExtra("semvideo");
            if ("1".equals(semVideo) || getIntent().getIntExtra("semvideo", 0) == 1) {
                Nucleo.semVideo(true);
            }
            if (direto != null) {
                conectar(direto);
            }
        }
    }

    private TextView texto(String t, int sp) {
        TextView v = new TextView(this);
        v.setText(t);
        v.setTextSize(sp);
        return v;
    }

    private Button botao(String t) {
        Button b = new Button(this);
        b.setText(t);
        b.setAllCaps(false);
        return b;
    }

    private void montarTelas() {
        raiz = new FrameLayout(this);

        telaLista = new ScrollView(this);
        LinearLayout lista = new LinearLayout(this);
        lista.setOrientation(LinearLayout.VERTICAL);
        lista.setPadding(32, 32, 32, 32);
        lista.addView(texto("PiStick", 28));
        Button procurar = botao("Procurar aparelhos na rede");
        procurar.setOnClickListener(v -> procurar());
        lista.addView(procurar);
        estadoLista = texto("Toque em \"Procurar\" ou digite o endereço do aparelho.", 16);
        lista.addView(estadoLista);
        aparelhos = new LinearLayout(this);
        aparelhos.setOrientation(LinearLayout.VERTICAL);
        lista.addView(aparelhos);
        lista.addView(texto("Endereço do aparelho", 16));
        campoEndereco = new EditText(this);
        campoEndereco.setHint("192.168.0.13 ou 192.168.0.13:7700");
        campoEndereco.setSingleLine(true);
        campoEndereco.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        lista.addView(campoEndereco);
        Button conectar = botao("Conectar");
        conectar.setOnClickListener(v -> conectar(campoEndereco.getText().toString().trim()));
        lista.addView(conectar);
        telaLista.addView(lista);

        telaPareamento = new LinearLayout(this);
        telaPareamento.setOrientation(LinearLayout.VERTICAL);
        telaPareamento.setGravity(Gravity.CENTER);
        telaPareamento.setPadding(32, 32, 32, 32);
        estadoPareamento = texto("", 18);
        estadoPareamento.setGravity(Gravity.CENTER);
        telaPareamento.addView(estadoPareamento);
        codigoPareamento = texto("", 40);
        codigoPareamento.setGravity(Gravity.CENTER);
        telaPareamento.addView(codigoPareamento);
        Button cancelar = botao("Cancelar");
        cancelar.setOnClickListener(v -> voltarParaLista("Conexão cancelada."));
        telaPareamento.addView(cancelar);

        telaSessao = new LinearLayout(this);
        telaSessao.setOrientation(LinearLayout.VERTICAL);
        HorizontalScrollView rolagemAbas = new HorizontalScrollView(this);
        abas = new LinearLayout(this);
        abas.setOrientation(LinearLayout.HORIZONTAL);
        rolagemAbas.addView(abas);
        telaSessao.addView(rolagemAbas, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        avisoSessao = texto("", 12);
        telaSessao.addView(avisoSessao);
        superficie = new SurfaceView(this);
        superficie.getHolder().addCallback(this);
        superficie.setOnTouchListener((v, e) -> {
            enviarToque(e);
            return true;
        });
        telaSessao.addView(superficie, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
        entrada = new EntradaView(this);
        entrada.aoEnviar(this::atualizarModificadores);
        telaSessao.addView(entrada, new LinearLayout.LayoutParams(1, 1));
        HorizontalScrollView rolagemTeclas = new HorizontalScrollView(this);
        LinearLayout faixa = new LinearLayout(this);
        faixa.setOrientation(LinearLayout.HORIZONTAL);
        botaoMenos = botao("A-");
        botaoMenos.setOnClickListener(v -> mudarEscala(-ESCALA_PASSO));
        faixa.addView(botaoMenos);
        botaoMais = botao("A+");
        botaoMais.setOnClickListener(v -> mudarEscala(ESCALA_PASSO));
        faixa.addView(botaoMais);
        Button teclado = botao("Teclado");
        teclado.setOnClickListener(v -> alternarTeclado());
        faixa.addView(teclado);
        faixa.addView(teclaExtra("Esc", KEY_ESC));
        faixa.addView(teclaExtra("Tab", KEY_TAB));
        botaoCtrl = botao("Ctrl: desligado");
        botaoCtrl.setOnClickListener(v -> botaoCtrl.setText(Nucleo.modificador(0) ? "Ctrl: LIGADO" : "Ctrl: desligado"));
        faixa.addView(botaoCtrl);
        botaoAlt = botao("Alt: desligado");
        botaoAlt.setOnClickListener(v -> botaoAlt.setText(Nucleo.modificador(1) ? "Alt: LIGADO" : "Alt: desligado"));
        faixa.addView(botaoAlt);
        faixa.addView(teclaExtra("←", KEY_LEFT));
        faixa.addView(teclaExtra("↑", KEY_UP));
        faixa.addView(teclaExtra("↓", KEY_DOWN));
        faixa.addView(teclaExtra("→", KEY_RIGHT));
        faixa.addView(teclaExtra("Home", KEY_HOME));
        faixa.addView(teclaExtra("End", KEY_END));
        Button sair = botao("Desconectar");
        sair.setOnClickListener(v -> voltarParaLista("Desconectado."));
        faixa.addView(sair);
        rolagemTeclas.addView(faixa);
        telaSessao.addView(rolagemTeclas, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        raiz.addView(telaLista);
        raiz.addView(telaPareamento);
        raiz.addView(telaSessao);
        setContentView(raiz);
    }

    private Button teclaExtra(String rotulo, int codigo) {
        Button b = botao(rotulo);
        b.setOnClickListener(v -> {
            Nucleo.teclaExtra(codigo);
            atualizarModificadores();
        });
        return b;
    }

    private void mudarEscala(int delta) {
        escala = Math.max(ESCALA_MIN, Math.min(ESCALA_MAX, escala + delta));
        aplicarEscala(true);
    }

    private void aplicarEscala(boolean guardar) {
        if (guardar) {
            getSharedPreferences(PREFS, MODE_PRIVATE).edit().putInt(PREF_ESCALA, escala).apply();
        }
        Nucleo.escala(escala);
        botaoMenos.setEnabled(escala > ESCALA_MIN);
        botaoMais.setEnabled(escala < ESCALA_MAX);
    }

    private void atualizarModificadores() {
        int m = Nucleo.modificadores();
        botaoCtrl.setText((m & 1) != 0 ? "Ctrl: LIGADO" : "Ctrl: desligado");
        botaoAlt.setText((m & 2) != 0 ? "Alt: LIGADO" : "Alt: desligado");
    }

    private void mostrar(View v) {
        Log.i(ETIQUETA, "tela: " + (v == telaLista ? "lista" : v == telaPareamento ? "conexão/pareamento" : "sessão"));
        telaLista.setVisibility(v == telaLista ? View.VISIBLE : View.GONE);
        telaPareamento.setVisibility(v == telaPareamento ? View.VISIBLE : View.GONE);
        telaSessao.setVisibility(v == telaSessao ? View.VISIBLE : View.GONE);
        emSessao = v == telaSessao;
        if (!emSessao) {
            esconderTeclado();
        }
    }

    private void voltarParaLista(String mensagem) {
        Log.i(ETIQUETA, "voltando à lista: " + mensagem);
        reconectar = false;
        reconectarAoVoltar = false;
        devolverFocoDeAudio();
        Nucleo.desconectar();
        soltarMulticast();
        estadoLista.setText(mensagem);
        mostrar(telaLista);
    }

    private void procurar() {
        aparelhos.removeAllViews();
        estadoLista.setText("Procurando aparelhos na rede…");
        if (travaMulticast == null) {
            WifiManager wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            if (wm != null) {
                travaMulticast = wm.createMulticastLock("pistick-mdns");
                travaMulticast.setReferenceCounted(false);
            }
        }
        if (travaMulticast != null && !travaMulticast.isHeld()) {
            travaMulticast.acquire();
        }
        Nucleo.buscar();
    }

    private void soltarMulticast() {
        if (travaMulticast != null && travaMulticast.isHeld()) {
            travaMulticast.release();
        }
    }

    private void conectar(String endereco) {
        if (endereco.isEmpty()) {
            estadoLista.setText("Digite o endereço do aparelho.");
            return;
        }
        Log.i(ETIQUETA, "conectando a " + endereco);
        ultimoEndereco = endereco;
        byte[] chave;
        try {
            chave = ChaveDoCliente.carregar(this);
        } catch (ChaveDoCliente.Ilegivel e) {
            Log.e(ETIQUETA, "chave do cliente ilegível: " + e.getMessage());
            estadoLista.setText("Erro: " + e.getMessage() + ". Nada foi sobrescrito e nenhuma chave nova foi criada. Se não há como recuperar, apague os dados do aplicativo e pareie de novo.");
            mostrar(telaLista);
            return;
        }
        estadoPareamento.setText("Conectando a " + endereco + "…");
        codigoPareamento.setText("");
        mostrar(telaPareamento);
        reconectar = true;
        Nucleo.conectar(endereco, chave);
        if (chave != null) {
            Arrays.fill(chave, (byte) 0);
        }
    }

    private void pedirFocoDeAudio() {
        AudioManager am = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
        if (am == null || focoDeAudio != null) {
            return;
        }
        AudioAttributes attrs = new AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_MEDIA)
                .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                .build();
        focoDeAudio = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                .setAudioAttributes(attrs)
                .setOnAudioFocusChangeListener(mudanca -> Log.i(ETIQUETA, "foco de áudio: " + mudanca))
                .build();
        int r = am.requestAudioFocus(focoDeAudio);
        Log.i(ETIQUETA, "foco de áudio pedido: " + (r == AudioManager.AUDIOFOCUS_REQUEST_GRANTED ? "concedido" : "negado"));
    }

    private void devolverFocoDeAudio() {
        if (focoDeAudio == null) {
            return;
        }
        AudioManager am = (AudioManager) getSystemService(Context.AUDIO_SERVICE);
        if (am != null) {
            am.abandonAudioFocusRequest(focoDeAudio);
            Log.i(ETIQUETA, "foco de áudio devolvido");
        }
        focoDeAudio = null;
    }

    private void alternarTeclado() {
        InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
        if (imm == null) {
            return;
        }
        if (tecladoAberto) {
            esconderTeclado();
        } else {
            entrada.requestFocus();
            tecladoAberto = imm.showSoftInput(entrada, 0);
        }
    }

    private void esconderTeclado() {
        tecladoAberto = false;
        InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
        if (imm != null && raiz != null) {
            imm.hideSoftInputFromWindow(raiz.getWindowToken(), 0);
        }
    }

    @Override
    protected void onStart() {
        super.onStart();
        if (raiz == null) {
            return;
        }
        Log.i(ETIQUETA, "onStart (reconectarAoVoltar=" + reconectarAoVoltar + ", endereço=" + ultimoEndereco + ")");
        noPrimeiroPlano = true;
        quadrosAtivos = true;
        Choreographer.getInstance().postFrameCallback(this);
        if (reconectarAoVoltar && ultimoEndereco != null) {
            reconectarAoVoltar = false;
            Log.i(ETIQUETA, "voltou ao app: reconectando a " + ultimoEndereco);
            conectar(ultimoEndereco);
        }
    }

    @Override
    protected void onStop() {
        super.onStop();
        if (raiz == null) {
            return;
        }
        Log.i(ETIQUETA, "onStop (emSessao=" + emSessao + ", reconectar=" + reconectar + ")");
        noPrimeiroPlano = false;
        quadrosAtivos = false;
        soltarMulticast();
        devolverFocoDeAudio();
        if (reconectar && (emSessao || telaPareamento.getVisibility() == View.VISIBLE)) {
            reconectarAoVoltar = true;
            Log.i(ETIQUETA, "fechando a conexão ao sair da frente; reconecta ao voltar");
            Nucleo.desconectar();
            estadoPareamento.setText("Conexão fechada ao sair do aplicativo. Reconectando ao voltar…");
            codigoPareamento.setText("");
            mostrar(telaPareamento);
        }
    }

    @Override
    public void onWindowFocusChanged(boolean temFoco) {
        super.onWindowFocusChanged(temFoco);
        if (raiz == null) {
            return;
        }
        Nucleo.focoApp(temFoco, temFoco ? textoDaArea() : null);
    }

    private String textoDaArea() {
        ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        if (cm == null || !cm.hasPrimaryClip() || cm.getPrimaryClip() == null || cm.getPrimaryClip().getItemCount() == 0) {
            return null;
        }
        CharSequence t = cm.getPrimaryClip().getItemAt(0).coerceToText(this);
        return t == null ? null : t.toString();
    }

    @Override
    public void surfaceCreated(SurfaceHolder h) {
        Log.i(ETIQUETA, "superfície criada");
        Nucleo.superficie(h.getSurface());
    }

    @Override
    public void surfaceChanged(SurfaceHolder h, int formato, int largura, int altura) {
        Log.i(ETIQUETA, "superfície " + largura + "x" + altura);
        Nucleo.tela(largura, altura, getResources().getDisplayMetrics().densityDpi);
        Nucleo.superficie(h.getSurface());
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder h) {
        Log.i(ETIQUETA, "superfície destruída");
        Nucleo.superficie(null);
    }

    private void enviarToque(MotionEvent e) {
        int n = Math.min(e.getPointerCount(), 10);
        float[] dados = new float[n * 3];
        for (int i = 0; i < n; i++) {
            dados[i * 3] = e.getPointerId(i);
            dados[i * 3 + 1] = e.getX(i);
            dados[i * 3 + 2] = e.getY(i);
        }
        Nucleo.toque(e.getActionMasked(), e.getActionIndex(), dados, e.getEventTime());
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent e) {
        if (emSessao && e.getScanCode() > 0 && fisico(e)) {
            int c = e.getKeyCode();
            if (c != KeyEvent.KEYCODE_BACK && c != KeyEvent.KEYCODE_HOME && c != KeyEvent.KEYCODE_VOLUME_UP && c != KeyEvent.KEYCODE_VOLUME_DOWN && c != KeyEvent.KEYCODE_VOLUME_MUTE && c != KeyEvent.KEYCODE_POWER) {
                if (e.getRepeatCount() == 0) {
                    Nucleo.tecla(e.getScanCode(), e.getAction() == KeyEvent.ACTION_DOWN);
                    atualizarModificadores();
                }
                return true;
            }
        }
        if (emSessao && e.getScanCode() == 0 && (e.getFlags() & KeyEvent.FLAG_SOFT_KEYBOARD) == 0 && !teclaDeSistema(e.getKeyCode())) {
            if (e.getRepeatCount() > 0) {
                return true;
            }
            boolean descer = e.getAction() == KeyEvent.ACTION_DOWN;
            if (Nucleo.teclaAndroid(e.getKeyCode(), descer)) {
                atualizarModificadores();
                return true;
            }
            int u = e.getUnicodeChar();
            if (u >= 32 && Character.isValidCodePoint(u)) {
                if (descer) {
                    Nucleo.texto(new String(Character.toChars(u)));
                    atualizarModificadores();
                }
                return true;
            }
        }
        return super.dispatchKeyEvent(e);
    }

    private static boolean teclaDeSistema(int c) {
        return c == KeyEvent.KEYCODE_BACK || c == KeyEvent.KEYCODE_HOME || c == KeyEvent.KEYCODE_VOLUME_UP || c == KeyEvent.KEYCODE_VOLUME_DOWN
                || c == KeyEvent.KEYCODE_VOLUME_MUTE || c == KeyEvent.KEYCODE_POWER || c == KeyEvent.KEYCODE_APP_SWITCH || c == KeyEvent.KEYCODE_UNKNOWN;
    }

    private static boolean fisico(KeyEvent e) {
        InputDevice d = e.getDevice();
        return d != null && !d.isVirtual() && (e.getSource() & InputDevice.SOURCE_KEYBOARD) == InputDevice.SOURCE_KEYBOARD;
    }

    @Override
    public void doFrame(long nanos) {
        if (!quadrosAtivos) {
            return;
        }
        byte[] b = Nucleo.poll(SystemClock.uptimeMillis());
        if (b != null && b.length > 0) {
            try {
                processar(b);
            } catch (RuntimeException e) {
                Log.e(ETIQUETA, "evento do núcleo ilegível: " + e);
            }
            Arrays.fill(b, (byte) 0);
        }
        Choreographer.getInstance().postFrameCallback(this);
    }

    private static final class Leitor {
        private final byte[] b;
        int p;

        Leitor(byte[] b, int p) {
            this.b = b;
            this.p = p;
        }

        int u8() {
            return b[p++] & 0xFF;
        }

        int u16() {
            int v = (b[p] & 0xFF) | ((b[p + 1] & 0xFF) << 8);
            p += 2;
            return v;
        }

        long u32() {
            long v = (b[p] & 0xFFL) | ((b[p + 1] & 0xFFL) << 8) | ((b[p + 2] & 0xFFL) << 16) | ((b[p + 3] & 0xFFL) << 24);
            p += 4;
            return v;
        }

        String texto() {
            int n = (int) u32();
            String s = new String(b, p, n, StandardCharsets.UTF_8);
            p += n;
            return s;
        }
    }

    private void processar(byte[] b) {
        int pos = 0;
        while (pos + 5 <= b.length) {
            int tipo = b[pos] & 0xFF;
            int tam = (b[pos + 1] & 0xFF) | ((b[pos + 2] & 0xFF) << 8) | ((b[pos + 3] & 0xFF) << 16) | ((b[pos + 4] & 0xFF) << 24);
            int ini = pos + 5;
            if (tam < 0 || ini + tam > b.length) {
                Log.e(ETIQUETA, "registro truncado do núcleo");
                return;
            }
            Leitor r = new Leitor(b, ini);
            switch (tipo) {
                case 1: {
                    int fase = r.u8();
                    estado(fase, r.texto());
                    break;
                }
                case 2: {
                    String nome = r.texto();
                    int seguro = r.u8();
                    int aberto = r.u8();
                    int n = r.u16();
                    List<String> enderecos = new ArrayList<>();
                    for (int i = 0; i < n; i++) {
                        enderecos.add(r.texto());
                    }
                    aparelho(nome, enderecos, seguro, aberto);
                    break;
                }
                case 3:
                    buscaFim((int) r.u32());
                    break;
                case 4:
                    codigoPareamento.setText(r.texto());
                    break;
                case 5: {
                    byte[] k = Arrays.copyOfRange(b, ini, ini + 32);
                    Nucleo.chaveGravada(ChaveDoCliente.gravar(this, k));
                    break;
                }
                case 6: {
                    Log.e(ETIQUETA, "erro do núcleo: " + r.texto());
                    r.p = ini;
                    voltarParaLista("Erro: " + r.texto());
                    break;
                }
                case 7:
                    janelas(r);
                    break;
                case 8: {
                    ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
                    if (cm != null) {
                        cm.setPrimaryClip(ClipData.newPlainText("PiStick", r.texto()));
                    }
                    break;
                }
                case 9:
                    superficie.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS);
                    break;
                case 10:
                    avisoSessao.setText(r.texto());
                    break;
                case 11:
                    Log.i(ETIQUETA, "sessão encerrada pelo aparelho, motivo " + r.u8() + ": " + r.texto());
                    break;
                case 12: {
                    long fl = r.u32();
                    StringBuilder sb = new StringBuilder();
                    if ((fl & 1) != 0) sb.append("Energia fraca (subtensão) no aparelho. ");
                    if ((fl & 4) != 0) sb.append("Temperatura alta no aparelho. ");
                    if ((fl & 8) != 0) sb.append("Modo descartável: o que os apps gravarem some.");
                    avisoSessao.setText(sb.toString().trim());
                    break;
                }
                default:
                    break;
            }
            pos = ini + tam;
        }
    }

    private void estado(int fase, String texto) {
        Log.i(ETIQUETA, "estado " + fase + ": " + texto);
        switch (fase) {
            case FASE_BUSCANDO:
                estadoLista.setText(texto);
                break;
            case FASE_CONECTANDO:
                estadoPareamento.setText(texto);
                break;
            case FASE_PAREANDO:
                estadoPareamento.setText(texto + "\n\nCódigo de pareamento (tem de ser o mesmo que o aparelho mostra):");
                mostrar(telaPareamento);
                break;
            case FASE_CONECTADO:
                if (!reconectar) {
                    Log.w(ETIQUETA, "estado Conectado ignorado: nenhuma conexão foi pedida (evento velho)");
                    break;
                }
                avisoSessao.setText(texto);
                mostrar(telaSessao);
                pedirFocoDeAudio();
                break;
            case FASE_DESCONECTADO:
                if (emSessao) {
                    voltarParaLista("Desconectado: " + texto);
                }
                break;
            case FASE_ERRO:
            case FASE_OCIOSO:
            default:
                break;
        }
    }

    private void aparelho(String nome, List<String> enderecos, int seguro, int aberto) {
        if (enderecos.isEmpty()) {
            return;
        }
        final String primeiro = enderecos.get(0);
        String pareamento = aberto == 1 ? "pareamento aberto" : (aberto == 0 ? "pareamento fechado" : "pareamento: desconhecido");
        Button b = botao(nome + "\n" + String.join(", ", enderecos) + "\n" + pareamento);
        b.setOnClickListener(v -> {
            campoEndereco.setText(primeiro);
            conectar(primeiro);
        });
        aparelhos.addView(b);
    }

    private void buscaFim(int achados) {
        soltarMulticast();
        estadoLista.setText(achados == 0 ? "Nenhum aparelho encontrado. Confira o Wi-Fi ou digite o endereço." : achados + (achados == 1 ? " aparelho encontrado." : " aparelhos encontrados."));
    }

    private void janelas(Leitor r) {
        int foco = (int) r.u32();
        int n = r.u16();
        abas.removeAllViews();
        for (int i = 0; i < n; i++) {
            final int id = (int) r.u32();
            String titulo = r.texto();
            if (titulo.isEmpty()) {
                titulo = "janela " + id;
            }
            Button b = botao(id == foco ? "[ " + titulo + " ] em foco" : titulo);
            b.setOnClickListener(v -> Nucleo.aba(id));
            abas.addView(b);
        }
        focoAtual = foco;
        if (foco == 0 && emSessao) {
            avisoSessao.setText("Nenhuma janela aberta no aparelho.");
        }
    }
}
