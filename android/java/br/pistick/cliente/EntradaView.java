package br.pistick.cliente;

import android.content.Context;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;

/**
 * Visão invisível que recebe o texto do teclado virtual (docs/26, decisão 11). O campo é declarado sem sugestões e sem
 * correção (NO_SUGGESTIONS | VISIBLE_PASSWORD) para o texto sair letra a letra. O núcleo converte o texto em batidas
 * ABNT2; aqui só se repassa o que o método de entrada confirma.
 */
final class EntradaView extends View {
    /** O que já foi enviado do trecho em composição (as IMEs que compõem texto o reescrevem). */
    private String composicao = "";

    /** Chamado depois de cada texto ou tecla enviados ao núcleo (que pode ter soltado Ctrl e Alt grudentos). */
    private Runnable aoEnviar;

    EntradaView(Context ctx) {
        super(ctx);
        setFocusable(true);
        setFocusableInTouchMode(true);
    }

    void aoEnviar(Runnable r) {
        aoEnviar = r;
    }

    private void enviado() {
        if (aoEnviar != null) {
            aoEnviar.run();
        }
    }

    @Override
    public boolean onCheckIsTextEditor() {
        return true;
    }

    @Override
    public InputConnection onCreateInputConnection(EditorInfo info) {
        info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD;
        info.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_ACTION_NONE;
        composicao = "";
        return new BaseInputConnection(this, false) {
            @Override
            public boolean commitText(CharSequence texto, int novaPosicao) {
                aplicar(texto.toString());
                composicao = "";
                return true;
            }

            @Override
            public boolean setComposingText(CharSequence texto, int novaPosicao) {
                aplicar(texto.toString());
                return true;
            }

            @Override
            public boolean finishComposingText() {
                composicao = "";
                return true;
            }

            @Override
            public boolean deleteSurroundingText(int antes, int depois) {
                composicao = "";
                Nucleo.apagar(antes, depois);
                enviado();
                return true;
            }

            @Override
            public boolean sendKeyEvent(KeyEvent e) {
                if (e.getAction() == KeyEvent.ACTION_DOWN) {
                    if (e.getKeyCode() == KeyEvent.KEYCODE_DEL) {
                        composicao = "";
                        Nucleo.apagar(1, 0);
                    } else if (e.getKeyCode() == KeyEvent.KEYCODE_FORWARD_DEL) {
                        Nucleo.apagar(0, 1);
                    } else if (e.getKeyCode() == KeyEvent.KEYCODE_ENTER) {
                        composicao = "";
                        Nucleo.texto("\n");
                    }
                    enviado();
                }
                return true;
            }
        };
    }

    /** Troca o trecho já enviado pelo novo: apaga o que diverge e digita o resto. */
    private void aplicar(String novo) {
        int p = 0;
        int max = Math.min(composicao.length(), novo.length());
        while (p < max && composicao.charAt(p) == novo.charAt(p)) {
            p++;
        }
        if (composicao.length() > p) {
            Nucleo.apagar(composicao.codePointCount(p, composicao.length()), 0);
        }
        if (novo.length() > p) {
            Nucleo.texto(novo.substring(p));
        }
        composicao = novo;
        enviado();
    }
}
