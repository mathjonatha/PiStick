#!/usr/bin/python3
"""Mede, no Pi, o instante (relógio do sistema, segundos desde 1970) em que cada pulso de som chega a um app que
grava da fonte pistick-mic. Marco 7B, aceite 5 (veja tools/roteiros/76-microfone-atraso.txt).

Uso: atraso-microfone.py SEGUNDOS   (XDG_RUNTIME_DIR do PipeWire do psd no ambiente)
Imprime `ONSET <segundos.milissegundos>` por pulso: pico acima de 4000 depois de 150 ms de quase silêncio.
Só olha a amplitude; não guarda nenhum áudio.
"""
import os, struct, subprocess, sys, time

dur = float(sys.argv[1])
p = subprocess.Popen(["pw-record", "--rate", "48000", "--channels", "2", "--format", "s16", "--target", "pistick-mic", "-"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
fd = p.stdout.fileno()
os.read(fd, 44)
t_end = time.time() + dur
quiet = 0
while time.time() < t_end:
    d = os.read(fd, 1920)
    t = time.time()
    if not d:
        break
    n = len(d) // 4
    s = struct.unpack("<%dh" % (n * 2), d[: n * 4])
    idx = next((i for i, v in enumerate(s) if abs(v) > 4000), None)
    if idx is None:
        quiet += n
        continue
    if quiet >= 7200:
        print("ONSET %.4f" % (t - (n - idx // 2) / 48000.0), flush=True)
    quiet = 0
p.terminate()
