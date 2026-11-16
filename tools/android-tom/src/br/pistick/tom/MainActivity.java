package br.pistick.tom;

import android.app.Activity;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.AudioTrack;
import android.os.Bundle;
import android.widget.TextView;

public class MainActivity extends Activity {
    private static final int TAXA = 48000;
    private static final double AMPLITUDE = 0.25;
    private static final int SEGUNDOS = 60;

    private volatile boolean tocando = true;
    private Thread fio;

    @Override
    protected void onCreate(Bundle estado) {
        super.onCreate(estado);
        TextView t = new TextView(this);
        t.setText("Tom PiStick: 440 Hz (esquerda) e 880 Hz (direita)");
        t.setTextSize(20f);
        setContentView(t);
        AudioManager am = getSystemService(AudioManager.class);
        am.setStreamVolume(AudioManager.STREAM_MUSIC, am.getStreamMaxVolume(AudioManager.STREAM_MUSIC), 0);
        fio = new Thread(this::tocar, "tom");
        fio.start();
    }

    private void tocar() {
        int tamanho = AudioTrack.getMinBufferSize(TAXA, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
        AudioTrack faixa = new AudioTrack.Builder()
                .setAudioAttributes(new AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build())
                .setAudioFormat(new AudioFormat.Builder()
                        .setSampleRate(TAXA)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                        .build())
                .setBufferSizeInBytes(Math.max(tamanho, TAXA / 10 * 4))
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build();
        faixa.setVolume(1.0f);
        faixa.play();
        short[] bloco = new short[TAXA / 50 * 2];
        long n = 0;
        long total = (long) TAXA * SEGUNDOS;
        while (tocando && n < total) {
            for (int i = 0; i < bloco.length; i += 2, n++) {
                double t = (double) n / TAXA;
                bloco[i] = (short) Math.round(AMPLITUDE * 32767 * Math.sin(2 * Math.PI * 440 * t));
                bloco[i + 1] = (short) Math.round(AMPLITUDE * 32767 * Math.sin(2 * Math.PI * 880 * t));
            }
            faixa.write(bloco, 0, bloco.length);
        }
        faixa.stop();
        faixa.release();
    }

    @Override
    protected void onDestroy() {
        tocando = false;
        super.onDestroy();
    }
}
