#!/usr/bin/env python3
# probe_crypto's result lines as a host computes them. MIT.
#
# The cross-check of the Rust reference `host_crypto` in tests/milestones/m8.rs: this script uses
# Python's `cryptography` package (OpenSSL) for AES and Python integers for RSA and MPI, and
# prints the lines the probe prints, in order. The two must agree line for line:
#
#   python3 tools/probe_crypto/host_expected.py
#
# Needs `cryptography` (pip install cryptography). No device and no emulator.

from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


def fill(n, seed):
    x, out = seed, bytearray()
    while len(out) < n:
        x ^= (x << 13) & 0xFFFFFFFF
        x ^= x >> 17
        x ^= (x << 5) & 0xFFFFFFFF
        out += x.to_bytes(4, "little")
    return bytes(out[:n])


def fnv(data):
    h = 0xCBF29CE484222325
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def result(data):
    return f"|fnv={fnv(data):016x}|head={data[:16].hex()}|tail={data[-16:].hex()}"


def enc(key, mode, data):
    e = Cipher(algorithms.AES(key), mode).encryptor()
    return e.update(data) + e.finalize()


def modulus():
    n = bytearray(fill(256, 109))
    n[0] |= 0x80
    n[255] |= 0x01
    return int.from_bytes(n, "big")


def lines():
    plain = fill(4101, 100)
    out = []
    for name, bits, key_seed in (("aes_cbc128", 128, 101), ("aes_cbc256", 256, 103)):
        c = enc(fill(bits // 8, key_seed), modes.CBC(fill(16, 102)), plain[:4096])
        out.append(f"AES|{name}|rc=0|len=4096{result(c)}|iv={c[-16:].hex()}|dec_rc=0|dec_ok=1")

    key, nonce = fill(16, 104), fill(16, 105)
    c = enc(key, modes.CTR(nonce), plain)
    blocks = (len(plain) + 15) // 16
    last = (int.from_bytes(nonce, "big") + blocks) % (1 << 128)
    stream = enc(key, modes.ECB(), ((last - 1) % (1 << 128)).to_bytes(16, "big"))
    # IDF's DMA driver copies the whole zero-padded last block's output into the stream block
    # (esp_aes_dma_core.c, esp_aes_process_dma), so its first nc_off bytes are ciphertext.
    tail = len(plain) % 16
    stream = c[len(c) - tail:] + stream[tail:]
    out.append(
        f"AES|aes_ctr128|rc=0|len=4101{result(c)}|nc_off={len(plain) % 16}"
        f"|nonce={last.to_bytes(16, 'big').hex()}|stream={stream.hex()}"
    )
    out.append(f"AES|aes_ctr128_split|rc=0|len=4101|fnv={fnv(c):016x}|same=1")

    sealed = AESGCM(fill(16, 106)).encrypt(fill(12, 107), plain[:4096], fill(20, 108))
    c, tag = sealed[:-16], sealed[-16:]
    out.append(f"AES|aes_gcm128|rc=0|len=4096{result(c)}|tag={tag.hex()}|dec_rc=0|dec_ok=1")

    n = modulus()
    x = bytearray(fill(256, 110))
    x[0] &= 0x7F
    r = pow(int.from_bytes(x, "big"), 65537, n).to_bytes(256, "big")
    out.append(f"RSA|rsa_pub2048|rc=0{result(r)}")

    for name, a_len, sa, b_len, sb in (
        ("mpi_mul1024", 128, 113, 128, 114),
        ("mpi_mul1536", 192, 115, 192, 116),
        ("mpi_mul2048x1024", 256, 117, 128, 118),
    ):
        z = int.from_bytes(fill(a_len, sa), "big") * int.from_bytes(fill(b_len, sb), "big")
        out.append(f"MPI|{name}|rc=0{result(z.to_bytes(a_len + b_len, 'big'))}")

    a = bytearray(fill(256, 111))
    a[0] &= 0x7F
    z = pow(int.from_bytes(a, "big"), int.from_bytes(fill(32, 112), "big"), n)
    out.append(f"MPI|mpi_exp2048|rc=0{result(z.to_bytes(256, 'big'))}")
    return out


if __name__ == "__main__":
    print("\n".join(lines()))
