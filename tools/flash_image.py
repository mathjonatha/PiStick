"""Grava uma imagem .img.xz direto num disco físico do Windows e confere a gravação.

Uso (precisa de administrador):  python -I flash_image.py <imagem.img.xz> <\\\\.\\PhysicalDriveN>
O disco precisa estar sem volumes montados (rode Clear-Disk antes).
"""
import hashlib
import lzma
import sys
import time

CHUNK = 4 * 1024 * 1024  # múltiplo de 512, exigido para escrita crua
SECTOR = 512


def main() -> int:
    image, device = sys.argv[1], sys.argv[2]
    written = 0
    h_src = hashlib.sha256()
    t0 = time.time()
    last = t0
    with lzma.open(image, "rb") as src, open(device, "r+b", buffering=0) as dst:
        while True:
            buf = src.read(CHUNK)
            if not buf:
                break
            if len(buf) % SECTOR:
                buf += b"\0" * (SECTOR - len(buf) % SECTOR)
            h_src.update(buf)
            dst.write(buf)
            written += len(buf)
            if time.time() - last > 5:
                last = time.time()
                print(f"gravado {written / 2**20:8.0f} MiB  ({written / 2**20 / (last - t0):.1f} MiB/s)", flush=True)
    print(f"gravacao concluida: {written} bytes em {time.time() - t0:.0f}s", flush=True)

    h_dst = hashlib.sha256()
    left = written
    with open(device, "rb", buffering=0) as dst:
        while left:
            buf = dst.read(min(CHUNK, left))
            if not buf:
                break
            h_dst.update(buf)
            left -= len(buf)
    if left or h_src.digest() != h_dst.digest():
        print("ERRO: verificacao falhou (o cartao devolveu dados diferentes)", flush=True)
        return 2
    print(f"verificacao OK sha256={h_dst.hexdigest()}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
