/* CPU-bound RV32IMC benchmark kernel for the rv32-interp spike.
 * Mix chosen to resemble emulator-relevant firmware work:
 *   - CRC32 (bitwise; branchy shifts/xors)
 *   - integer matrix multiply (mul, array indexing)
 *   - RGB565 alpha blend + fill (LVGL-style pixel loops)
 *   - insertion sort on an LCG array (compare/branch/load/store)
 *   - a small byte-code state machine (switch dispatch, like protocol parsers)
 * Bare metal: no libc. The same file builds natively (host) to produce the
 * reference checksum. Entry: main(iterations) returns a 32-bit checksum.
 */
#include <stdint.h>

#define BUF_LEN 2048
#define MAT_N 20
#define W 240
#define H 32
#define SORT_N 160

static uint8_t buf[BUF_LEN];
static int32_t ma[MAT_N * MAT_N], mb[MAT_N * MAT_N], mc[MAT_N * MAT_N];
static uint16_t fb[W * H];
static uint16_t src[W * H];
static uint8_t alpha[W];
static int32_t arr[SORT_N];
static uint32_t lcg_state = 12345u;

static uint32_t lcg(void) {
    lcg_state = lcg_state * 1664525u + 1013904223u;
    return lcg_state;
}

static uint32_t crc32_bitwise(const uint8_t *p, uint32_t n, uint32_t crc) {
    crc = ~crc;
    for (uint32_t i = 0; i < n; i++) {
        crc ^= p[i];
        for (int k = 0; k < 8; k++) {
            uint32_t mask = -(crc & 1u);
            crc = (crc >> 1) ^ (0xEDB88320u & mask);
        }
    }
    return ~crc;
}

static uint32_t matmul(void) {
    for (int i = 0; i < MAT_N; i++)
        for (int j = 0; j < MAT_N; j++) {
            int32_t s = 0;
            for (int k = 0; k < MAT_N; k++)
                s += ma[i * MAT_N + k] * mb[k * MAT_N + j];
            mc[i * MAT_N + j] = s;
        }
    uint32_t h = 0;
    for (int i = 0; i < MAT_N * MAT_N; i++) h = (h * 31u) ^ (uint32_t)mc[i];
    return h;
}

static inline uint16_t blend565(uint16_t fg, uint16_t bg, uint8_t a) {
    uint32_t r = (((fg >> 11) & 31u) * a + ((bg >> 11) & 31u) * (255u - a)) / 255u;
    uint32_t g = (((fg >> 5) & 63u) * a + ((bg >> 5) & 63u) * (255u - a)) / 255u;
    uint32_t b = ((fg & 31u) * a + (bg & 31u) * (255u - a)) / 255u;
    return (uint16_t)((r << 11) | (g << 5) | b);
}

static uint32_t pixels(uint32_t seed) {
    uint16_t color = (uint16_t)(seed * 2654435761u >> 16);
    for (int i = 0; i < W * H; i++) fb[i] = color;
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++) {
            uint16_t *d = &fb[y * W + x];
            *d = blend565(src[y * W + x], *d, alpha[x]);
        }
    uint32_t h = 0;
    for (int i = 0; i < W * H; i += 7) h = h * 33u + fb[i];
    return h;
}

static uint32_t sort(void) {
    for (int i = 0; i < SORT_N; i++) arr[i] = (int32_t)(lcg() >> 8) - 0x800000;
    for (int i = 1; i < SORT_N; i++) {
        int32_t v = arr[i];
        int j = i - 1;
        while (j >= 0 && arr[j] > v) {
            arr[j + 1] = arr[j];
            j--;
        }
        arr[j + 1] = v;
    }
    uint32_t h = 0;
    for (int i = 0; i < SORT_N; i += 5) h ^= (uint32_t)arr[i] + (uint32_t)i;
    return h;
}

static const uint8_t prog[] = {1, 5, 2, 3, 4, 1, 7, 6, 2, 9, 5, 0, 3, 8, 4, 6, 1, 2, 7, 5, 9, 3, 0, 8};

static uint32_t vm(uint32_t steps) {
    int32_t acc = 1, reg = 3;
    uint32_t pc = 0;
    for (uint32_t s = 0; s < steps; s++) {
        uint8_t op = prog[pc % sizeof(prog)];
        switch (op) {
        case 0: acc += reg; break;
        case 1: acc -= 7; break;
        case 2: reg ^= acc; break;
        case 3: acc = acc * 3 + 1; break;
        case 4: reg = (reg << 3) | ((uint32_t)reg >> 29); break;
        case 5: if (acc & 1) pc += 2; break;
        case 6: acc = (int32_t)((uint32_t)acc / (uint32_t)((reg & 0xff) | 1)); break;
        case 7: reg = acc % 1000; break;
        case 8: acc = ~acc; break;
        default: acc ^= 0x5a5a5a5a; break;
        }
        pc++;
    }
    return (uint32_t)acc ^ ((uint32_t)reg << 1);
}

uint32_t bench_main(uint32_t iterations) {
    for (int i = 0; i < BUF_LEN; i++) buf[i] = (uint8_t)lcg();
    for (int i = 0; i < MAT_N * MAT_N; i++) {
        ma[i] = (int32_t)(lcg() % 199) - 99;
        mb[i] = (int32_t)(lcg() % 211) - 105;
    }
    for (int i = 0; i < W * H; i++) src[i] = (uint16_t)lcg();
    for (int i = 0; i < W; i++) alpha[i] = (uint8_t)(i * 255 / (W - 1));

    uint32_t check = 0;
    for (uint32_t it = 0; it < iterations; it++) {
        check = crc32_bitwise(buf, BUF_LEN, check);
        check ^= matmul();
        check = check * 7u + pixels(check);
        check ^= sort();
        check += vm(4000 + (check & 255));
        ma[it % (MAT_N * MAT_N)] ^= (int32_t)check;
        buf[it % BUF_LEN] ^= (uint8_t)check;
    }
    return check;
}

#ifdef HOST
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    uint32_t it = argc > 1 ? (uint32_t)strtoul(argv[1], 0, 0) : 10;
    printf("iterations=%u checksum=0x%08x\n", it, bench_main(it));
    return 0;
}
#endif
