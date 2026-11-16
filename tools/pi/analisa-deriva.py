#!/usr/bin/python3
"""Analisa uma gravação longa do tom do roteiro do microfone (440 Hz à esquerda, amplitude 0,25), marco 7B.

Uso: analisa-deriva.py ARQ.wav [FREQ_ESQ]
Imprime: duração; blocos de 10 ms quase silenciosos (buracos); e, por janela de 100 ms, os saltos de fase do tom
(amostras que o PipeWire jogou fora ou repetiu), com a soma deles em quadros. Uma deriva acumulada apareceria como
soma que cresce sempre para o mesmo lado e um atraso que aumenta; saltos pequenos e raros são o PipeWire absorvendo a
diferença entre o relógio de quem escreve e o do grafo. Só lê o arquivo de teste (tom sintético).
"""
import math, struct, sys

d = open(sys.argv[1], "rb").read()
f = float(sys.argv[2]) if len(sys.argv) > 2 else 440.0
d = d[d.find(b"data") + 8:]
n = len(d) // 4
s = struct.unpack("<%dh" % (n * 2), d[: n * 4])
L = s[0::2]
print("quadros %d  duracao %.3f s" % (n, n / 48000))
buracos = sum(1 for b in range(n // 480) if max(map(abs, s[b * 960:(b + 1) * 960])) < 6000)
print("blocos de 10 ms com pico < 6000 (buracos): %d de %d" % (buracos, n // 480))
c = [math.cos(2 * math.pi * f * k / 48000) for k in range(48000)]
si = [math.sin(2 * math.pi * f * k / 48000) for k in range(48000)]
B = 4800
prev = None
soma = 0.0
eventos = 0
pior = 0.0
reg_ev = 0
reg_soma = 0.0
for b in range(n // B):
    re = im = 0.0
    base = b * B
    for k in range(B):
        v = L[base + k]
        j = (base + k) % 48000
        re += v * c[j]
        im += v * si[j]
    ph = math.atan2(im, re)
    if prev is not None:
        dph = (ph - prev + math.pi) % (2 * math.pi) - math.pi
        if abs(dph) > 0.05:
            q = dph / (2 * math.pi * f / 48000)
            soma += q
            eventos += 1
            pior = max(pior, abs(q))
            if b * 0.1 >= 30.0:
                reg_ev += 1
                reg_soma += q
            print("  t=%.1f s: salto de %.1f quadros" % (b * 0.1, q))
    prev = ph
print("SALTOS eventos=%d soma=%.1f quadros (%.2f ms) maior=%.1f quadros" % (eventos, soma, soma / 48.0, pior))
print("SALTOS_REGIME (depois de 30 s) eventos=%d soma=%.1f quadros (%.2f ms)" % (reg_ev, reg_soma, reg_soma / 48.0))
