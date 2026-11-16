package br.pistick.cliente;

import android.content.Context;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.util.Arrays;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/**
 * A chave do cliente (docs/26, decisão 13): o arquivo "chave-do-cliente" na pasta privada do app guarda a chave
 * cifrada pelo Android Keystore (AES-256-GCM, chave do sistema não exportável, sem exigir biometria). O formato do
 * arquivo (PSAKS001 + nonce + bloco) é do núcleo: a casca só cifra e decifra e usa chaveDecompor/chaveComporArquivo.
 * Arquivo que não abre é erro claro; nunca se cria chave nova por cima.
 */
final class ChaveDoCliente {
    private static final String APELIDO = "pistick-chave-do-cliente";
    private static final String ARQUIVO = "chave-do-cliente";
    private static final int NONCE = 12;

    private ChaveDoCliente() {
    }

    static final class Ilegivel extends Exception {
        private static final long serialVersionUID = 1L;

        Ilegivel(String msg) {
            super(msg);
        }
    }

    private static SecretKey chaveDoSistema() throws GeneralSecurityException, IOException {
        KeyStore ks = KeyStore.getInstance("AndroidKeyStore");
        ks.load(null);
        if (ks.containsAlias(APELIDO)) {
            return (SecretKey) ks.getKey(APELIDO, null);
        }
        KeyGenerator g = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
        g.init(new KeyGenParameterSpec.Builder(APELIDO, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build());
        return g.generateKey();
    }

    /**
     * Os 32 bytes da chave, ou null se ainda não há arquivo (o núcleo cria uma). O chamador apaga o resultado com
     * Arrays.fill depois de entregá-lo ao núcleo.
     */
    static byte[] carregar(Context ctx) throws Ilegivel {
        File f = new File(ctx.getFilesDir(), ARQUIVO);
        if (!f.exists()) {
            return null;
        }
        byte[] arquivo = null;
        byte[] partes = null;
        try {
            arquivo = Files.readAllBytes(f.toPath());
            partes = Nucleo.chaveDecompor(arquivo);
            if (partes == null) {
                throw new Ilegivel("o arquivo da chave deste aparelho está corrompido");
            }
            byte[] nonce = Arrays.copyOfRange(partes, 0, NONCE);
            byte[] bloco = Arrays.copyOfRange(partes, NONCE, partes.length);
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.DECRYPT_MODE, chaveDoSistema(), new GCMParameterSpec(128, nonce));
            byte[] plano = c.doFinal(bloco);
            if (plano.length != 32) {
                Arrays.fill(plano, (byte) 0);
                throw new Ilegivel("a chave decifrada tem tamanho inválido");
            }
            return plano;
        } catch (GeneralSecurityException | IOException e) {
            throw new Ilegivel("o Keystore não conseguiu abrir a chave deste aparelho (" + e.getClass().getSimpleName() + ")");
        } finally {
            if (arquivo != null) {
                Arrays.fill(arquivo, (byte) 0);
            }
            if (partes != null) {
                Arrays.fill(partes, (byte) 0);
            }
        }
    }

    /** Cifra a chave nova e grava o arquivo (por arquivo temporário e troca). Apaga {@code chave} no fim. */
    static boolean gravar(Context ctx, byte[] chave) {
        byte[] arquivo = null;
        try {
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            c.init(Cipher.ENCRYPT_MODE, chaveDoSistema());
            byte[] iv = c.getIV();
            byte[] cifrado = c.doFinal(chave);
            byte[] junto = new byte[iv.length + cifrado.length];
            System.arraycopy(iv, 0, junto, 0, iv.length);
            System.arraycopy(cifrado, 0, junto, iv.length, cifrado.length);
            arquivo = Nucleo.chaveComporArquivo(junto);
            Arrays.fill(cifrado, (byte) 0);
            Arrays.fill(junto, (byte) 0);
            if (arquivo == null) {
                return false;
            }
            File tmp = new File(ctx.getFilesDir(), ARQUIVO + ".tmp");
            try (FileOutputStream out = new FileOutputStream(tmp)) {
                out.write(arquivo);
                out.getFD().sync();
            }
            Files.move(tmp.toPath(), new File(ctx.getFilesDir(), ARQUIVO).toPath(), StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            return true;
        } catch (GeneralSecurityException | IOException e) {
            return false;
        } finally {
            Arrays.fill(chave, (byte) 0);
            if (arquivo != null) {
                Arrays.fill(arquivo, (byte) 0);
            }
        }
    }
}
