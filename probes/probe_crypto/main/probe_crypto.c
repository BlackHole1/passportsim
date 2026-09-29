// probe_crypto: mbedTLS through the C3 crypto accelerators, AES in its DMA mode and the RSA / MPI
// block. MIT. An ordinary ESP-IDF v5.5.3 app with no
// emulator-specific code, built with the device's own partition table.
//
// Every input is derived from a fixed xorshift32 stream, so the results are deterministic and a
// host can compute them without a device: the milestone test that runs this image in the emulator
// recomputes every value with its own code and compares (tests/milestones/m8.rs,
// t1_m8_probe_crypto_matches_the_host_computed_values).
//
//   aes_cbc128, aes_cbc256   mbedtls_aes_crypt_cbc over 4096 bytes, encrypt then decrypt back;
//                            4096 bytes is over AES_DMA_INTR_TRIG_LEN, so the driver waits for the
//                            completion interrupt (source 48)
//   aes_ctr128               mbedtls_aes_crypt_ctr over 4101 bytes in one call (a partial last
//                            block, so the stream block and nc_off matter), and again in two calls
//                            of 1000 and 3101 bytes that continue through nc_off and the counter;
//                            IDF's DMA driver runs the zero-padded tail as one more block and
//                            copies its whole output into the stream block, so the first nc_off
//                            bytes of `stream` are ciphertext, not key stream
//   aes_gcm128               mbedtls_gcm_crypt_and_tag over 4096 bytes with 20 bytes of AAD, then
//                            mbedtls_gcm_auth_decrypt; the C3 has no GCM hardware, so this is
//                            IDF's GCM port (esp_aes_gcm.c): GHASH in software, H from one ECB
//                            block, the payload and the tag through CTR runs of the DMA mode
//   rsa_pub2048              mbedtls_rsa_public with a 2048-bit modulus and e = 65537 (the RSA
//                            block's MODEXP_START, search on)
//   mpi_mul1024, mpi_mul1536 mbedtls_mpi_mul_mpi of two 1024-bit and two 1536-bit numbers (the
//                            block's MULT_START; 1536 bits is its 48-word maximum)
//   mpi_mul2048x1024         a product too wide for MULT_START, which IDF runs as MOD_MULT_START
//                            over M = 2^3072 - 1 (esp_mpi_mult_mpi_failover_mod_mult_hw_op)
//   mpi_exp2048              mbedtls_mpi_exp_mod with a 2048-bit modulus and a 256-bit exponent
//
// Prints, as probe lines (probes/common/probe_line.h):
//   AES|<name>|len=..|fnv=..|head=..|tail=..|...   the FNV-1a 64 of the output, its first and
//                                                    last 16 bytes, and the chaining values
//   RSA|<name>|rc=..|fnv=..|head=..|tail=..
//   MPI|<name>|rc=..|fnv=..|head=..|tail=..         results as fixed-length big-endian bytes
//
// FNV-1a 64 (offset basis 0xcbf29ce484222325, prime 0x100000001b3) is used rather than a CRC or a
// SHA so that the check depends on no other accelerator and no ROM routine. No line carries a MAC,
// a unique id or anything device-specific. The probe writes no flash.

#include <inttypes.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "esp_heap_caps.h"
#include "mbedtls/aes.h"
#include "mbedtls/bignum.h"
#include "mbedtls/gcm.h"
#include "mbedtls/rsa.h"

#include "probe_line.h"

#define PROBE_NAME "probe_crypto"

#define LEN 4096u
#define CTR_LEN 4101u
#define CTR_SPLIT 1000u
#define AAD_LEN 20u
#define GCM_IV_LEN 12u
#define TAG_LEN 16u

static bool s_ok = true;

static void fail(const char *what, const char *detail)
{
    PROBE_FAIL(what, detail);
    s_ok = false;
}

// `len` bytes of the xorshift32 stream from `seed`: each step `x ^= x << 13; x ^= x >> 17;
// x ^= x << 5`, its four bytes little-endian.
static void fill(uint8_t *out, size_t len, uint32_t seed)
{
    uint32_t x = seed;
    for (size_t i = 0; i < len; i += 4) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        for (size_t j = 0; j < 4 && i + j < len; j++) {
            out[i + j] = (uint8_t)(x >> (8 * j));
        }
    }
}

static uint64_t fnv(const uint8_t *data, size_t len)
{
    uint64_t h = 0xcbf29ce484222325ull;
    for (size_t i = 0; i < len; i++) {
        h ^= data[i];
        h *= 0x100000001b3ull;
    }
    return h;
}

// `len` bytes as lowercase hex into `out`, which holds at least 2 * len + 1 bytes.
static const char *hex(char *out, const uint8_t *data, size_t len)
{
    for (size_t i = 0; i < len; i++) {
        snprintf(&out[2 * i], 3, "%02x", data[i]);
    }
    out[2 * len] = '\0';
    return out;
}

// The common tail of a result line: fnv, then the first and last 16 bytes.
static void print_result(const uint8_t *data, size_t len)
{
    char a[33];
    char b[33];
    printf("|fnv=%016" PRIx64 "|head=%s|tail=%s", fnv(data, len), hex(a, data, 16),
           hex(b, data + len - 16, 16));
}

static void aes_cbc(const char *name, unsigned bits, uint32_t key_seed, uint32_t iv_seed,
                    const uint8_t *plain, uint8_t *cipher, uint8_t *back)
{
    uint8_t key[32];
    uint8_t iv0[16];
    uint8_t iv[16];
    fill(key, sizeof key, key_seed);
    fill(iv0, sizeof iv0, iv_seed);
    mbedtls_aes_context ctx;
    mbedtls_aes_init(&ctx);
    memcpy(iv, iv0, 16);
    int rc = mbedtls_aes_setkey_enc(&ctx, key, bits);
    if (rc == 0) {
        rc = mbedtls_aes_crypt_cbc(&ctx, MBEDTLS_AES_ENCRYPT, LEN, iv, plain, cipher);
    }
    char h[33];
    printf("AES|%s|rc=%d|len=%u", name, rc, LEN);
    print_result(cipher, LEN);
    printf("|iv=%s", hex(h, iv, 16));
    memcpy(iv, iv0, 16);
    int rc2 = mbedtls_aes_setkey_dec(&ctx, key, bits);
    if (rc2 == 0) {
        rc2 = mbedtls_aes_crypt_cbc(&ctx, MBEDTLS_AES_DECRYPT, LEN, iv, cipher, back);
    }
    bool same = rc2 == 0 && memcmp(back, plain, LEN) == 0;
    printf("|dec_rc=%d|dec_ok=%d\n", rc2, same ? 1 : 0);
    mbedtls_aes_free(&ctx);
    if (rc != 0 || !same) {
        fail(name, "encrypt failed or the decryption did not give the plaintext back");
    }
}

static void aes_ctr(const uint8_t *plain, uint8_t *out)
{
    uint8_t key[16];
    uint8_t nonce0[16];
    uint8_t nonce[16];
    uint8_t stream[16];
    fill(key, sizeof key, 104);
    fill(nonce0, sizeof nonce0, 105);
    mbedtls_aes_context ctx;
    mbedtls_aes_init(&ctx);
    int rc = mbedtls_aes_setkey_enc(&ctx, key, 128);
    size_t off = 0;
    memcpy(nonce, nonce0, 16);
    memset(stream, 0, 16);
    if (rc == 0) {
        rc = mbedtls_aes_crypt_ctr(&ctx, CTR_LEN, &off, nonce, stream, plain, out);
    }
    char a[33];
    char b[33];
    printf("AES|aes_ctr128|rc=%d|len=%u", rc, CTR_LEN);
    print_result(out, CTR_LEN);
    printf("|nc_off=%u|nonce=%s|stream=%s\n", (unsigned)off, hex(a, nonce, 16), hex(b, stream, 16));
    uint64_t whole = fnv(out, CTR_LEN);

    // The same stream in two calls: the second continues from nc_off, the stream block and the
    // counter the first read back from IV_MEM.
    memset(out, 0, CTR_LEN);
    off = 0;
    memcpy(nonce, nonce0, 16);
    memset(stream, 0, 16);
    int rc2 = mbedtls_aes_crypt_ctr(&ctx, CTR_SPLIT, &off, nonce, stream, plain, out);
    if (rc2 == 0) {
        rc2 = mbedtls_aes_crypt_ctr(&ctx, CTR_LEN - CTR_SPLIT, &off, nonce, stream,
                                    plain + CTR_SPLIT, out + CTR_SPLIT);
    }
    uint64_t split = fnv(out, CTR_LEN);
    printf("AES|aes_ctr128_split|rc=%d|len=%u|fnv=%016" PRIx64 "|same=%d\n", rc2, CTR_LEN, split,
           split == whole ? 1 : 0);
    mbedtls_aes_free(&ctx);
    if (rc != 0 || rc2 != 0 || split != whole) {
        fail("aes_ctr128", "a call failed or the split stream differs from the whole one");
    }
}

static void aes_gcm(const uint8_t *plain, uint8_t *cipher, uint8_t *back)
{
    uint8_t key[16];
    uint8_t iv[GCM_IV_LEN];
    uint8_t aad[AAD_LEN];
    uint8_t tag[TAG_LEN];
    fill(key, sizeof key, 106);
    fill(iv, sizeof iv, 107);
    fill(aad, sizeof aad, 108);
    mbedtls_gcm_context ctx;
    mbedtls_gcm_init(&ctx);
    int rc = mbedtls_gcm_setkey(&ctx, MBEDTLS_CIPHER_ID_AES, key, 128);
    if (rc == 0) {
        rc = mbedtls_gcm_crypt_and_tag(&ctx, MBEDTLS_GCM_ENCRYPT, LEN, iv, sizeof iv, aad,
                                       sizeof aad, plain, cipher, sizeof tag, tag);
    }
    char h[33];
    printf("AES|aes_gcm128|rc=%d|len=%u", rc, LEN);
    print_result(cipher, LEN);
    printf("|tag=%s", hex(h, tag, 16));
    int rc2 = mbedtls_gcm_auth_decrypt(&ctx, LEN, iv, sizeof iv, aad, sizeof aad, tag, sizeof tag,
                                       cipher, back);
    bool same = rc2 == 0 && memcmp(back, plain, LEN) == 0;
    printf("|dec_rc=%d|dec_ok=%d\n", rc2, same ? 1 : 0);
    mbedtls_gcm_free(&ctx);
    if (rc != 0 || !same) {
        fail("aes_gcm128", "encrypt failed or the authenticated decryption did not verify");
    }
}

// The 2048-bit modulus of the RSA and exponentiation lines: the stream from seed 109, the top bit
// and the low bit set.
static void modulus(uint8_t n[256])
{
    fill(n, 256, 109);
    n[0] |= 0x80;
    n[255] |= 0x01;
}

static void rsa_public(uint8_t *out)
{
    uint8_t n[256];
    uint8_t x[256];
    const uint8_t e[3] = {0x01, 0x00, 0x01};
    modulus(n);
    fill(x, sizeof x, 110);
    x[0] &= 0x7f;
    mbedtls_rsa_context rsa;
    mbedtls_rsa_init(&rsa);
    int rc = mbedtls_rsa_import_raw(&rsa, n, sizeof n, NULL, 0, NULL, 0, NULL, 0, e, sizeof e);
    if (rc == 0) {
        rc = mbedtls_rsa_complete(&rsa);
    }
    if (rc == 0) {
        rc = mbedtls_rsa_public(&rsa, x, out);
    }
    printf("RSA|rsa_pub2048|rc=%d", rc);
    print_result(out, 256);
    printf("\n");
    mbedtls_rsa_free(&rsa);
    if (rc != 0) {
        fail("rsa_pub2048", "mbedtls_rsa_public failed");
    }
}

// Z = A * B with A and B `a_len` and `b_len` bytes of the stream from `sa` and `sb`, printed as
// `a_len + b_len` big-endian bytes.
static void mpi_mul(const char *name, size_t a_len, uint32_t sa, size_t b_len, uint32_t sb,
                    uint8_t *buf)
{
    mbedtls_mpi a, b, z;
    mbedtls_mpi_init(&a);
    mbedtls_mpi_init(&b);
    mbedtls_mpi_init(&z);
    fill(buf, a_len, sa);
    int rc = mbedtls_mpi_read_binary(&a, buf, a_len);
    fill(buf, b_len, sb);
    if (rc == 0) {
        rc = mbedtls_mpi_read_binary(&b, buf, b_len);
    }
    if (rc == 0) {
        rc = mbedtls_mpi_mul_mpi(&z, &a, &b);
    }
    if (rc == 0) {
        rc = mbedtls_mpi_write_binary(&z, buf, a_len + b_len);
    }
    printf("MPI|%s|rc=%d", name, rc);
    print_result(buf, a_len + b_len);
    printf("\n");
    mbedtls_mpi_free(&a);
    mbedtls_mpi_free(&b);
    mbedtls_mpi_free(&z);
    if (rc != 0) {
        fail(name, "mbedtls_mpi_mul_mpi failed");
    }
}

static void mpi_exp(uint8_t *buf)
{
    mbedtls_mpi a, e, n, z;
    mbedtls_mpi_init(&a);
    mbedtls_mpi_init(&e);
    mbedtls_mpi_init(&n);
    mbedtls_mpi_init(&z);
    uint8_t m[256];
    modulus(m);
    int rc = mbedtls_mpi_read_binary(&n, m, sizeof m);
    fill(buf, 256, 111);
    buf[0] &= 0x7f;
    if (rc == 0) {
        rc = mbedtls_mpi_read_binary(&a, buf, 256);
    }
    fill(buf, 32, 112);
    if (rc == 0) {
        rc = mbedtls_mpi_read_binary(&e, buf, 32);
    }
    if (rc == 0) {
        rc = mbedtls_mpi_exp_mod(&z, &a, &e, &n, NULL);
    }
    if (rc == 0) {
        rc = mbedtls_mpi_write_binary(&z, buf, 256);
    }
    printf("MPI|mpi_exp2048|rc=%d", rc);
    print_result(buf, 256);
    printf("\n");
    mbedtls_mpi_free(&a);
    mbedtls_mpi_free(&e);
    mbedtls_mpi_free(&n);
    mbedtls_mpi_free(&z);
    if (rc != 0) {
        fail("mpi_exp2048", "mbedtls_mpi_exp_mod failed");
    }
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    // Internal, DMA-capable buffers: the driver hands these to GDMA directly.
    const uint32_t caps = MALLOC_CAP_DMA | MALLOC_CAP_8BIT | MALLOC_CAP_INTERNAL;
    uint8_t *plain = heap_caps_malloc(CTR_LEN, caps);
    uint8_t *out = heap_caps_malloc(CTR_LEN, caps);
    uint8_t *back = heap_caps_malloc(CTR_LEN, caps);
    if (plain == NULL || out == NULL || back == NULL) {
        fail("alloc", "no 4 KB DMA-capable buffers");
        PROBE_END(PROBE_NAME, "fail");
        return;
    }
    fill(plain, CTR_LEN, 100);

    aes_cbc("aes_cbc128", 128, 101, 102, plain, out, back);
    aes_cbc("aes_cbc256", 256, 103, 102, plain, out, back);
    aes_ctr(plain, out);
    aes_gcm(plain, out, back);
    rsa_public(out);
    mpi_mul("mpi_mul1024", 128, 113, 128, 114, out);
    mpi_mul("mpi_mul1536", 192, 115, 192, 116, out);
    mpi_mul("mpi_mul2048x1024", 256, 117, 128, 118, out);
    mpi_exp(out);

    free(plain);
    free(out);
    free(back);
    PROBE_END(PROBE_NAME, s_ok ? "ok" : "fail");
}
