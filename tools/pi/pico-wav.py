#!/usr/bin/python3
"""Imprime `quadros N pico P` de um WAV de 16 bits (P = maior amplitude absoluta; 0 = silêncio total). Marco 7B, aceite 4."""
import struct, sys
d = open(sys.argv[1], "rb").read()
d = d[d.find(b"data") + 8:]
s = struct.unpack("<%dh" % (len(d) // 2), d[: len(d) // 2 * 2])
print("quadros", len(s) // 2, "pico", max(map(abs, s)) if s else -1)
