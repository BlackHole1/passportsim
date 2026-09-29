// probe_limits: the heap limits `limits-heap` and `limits-audio`.
// MIT. An ordinary ESP-IDF v5.5.3 app with no emulator-specific code.
//
// Two parts, run once from app_main:
//
//   1. For each capability set, allocate until an allocation fails, then free everything. The
//      first request of each round is the largest free block, then smaller and smaller requests
//      fill what is left, so the drained total is close to everything the allocator can hand
//      out. A walk of every heap before and after the drain attributes the drained bytes to the
//      heap region they came from. The `limits-heap` test compares these per region with a silicon
//      capture of this same image (the `IMAGE` line identifies it), within the allocator
//      overhead. The `pk` device regions of 117, 113, 10 and 7 KiB are context, not the reference:
//      the first DRAM region depends on each image's static data.
//   2. `malloc(96000)`, the one contiguous record buffer of the official audio demo (3 s of
//      16 kHz, 16-bit mono), must succeed; a request exactly the size of the largest
//      free block must succeed and one byte above it must fail.
//
// Prints, as probe lines (probes/common/probe_line.h):
//   IMAGE    the SHA-256 of the running app ELF, which a silicon capture must match before its
//            regions are compared
//   HEAPREG  one line per heap region: index, start, end, size
//   LIMIT    per capability set: bytes free and largest block before, number of allocations,
//            bytes requested and bytes usable, whether the drain ended in a refused allocation,
//            and whether freeing everything gave the free total back
//   LIMREG   per capability set and region: start, end and size, used bytes before and after the
//            drain, bytes drained, and whether the region shrank (a failure)
//   AUDIO    the 96000-byte request, with the largest free block before it
//   ABOVE    the request at and one byte above the largest free block
//
// Deterministic: nothing here reads a clock, a random number or an identity. Every number is a
// function of the image and the allocator, so two runs of the same image print the same lines.
//
// Consumed by: `limits-heap` in tests/milestones/m5.rs and `limits-audio` in m6.rs.

#include <inttypes.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

#include "esp_app_desc.h"
#include "esp_heap_caps.h"

#include "probe_line.h"

#define PROBE_NAME "probe_limits"

// The official audio demo's record buffer: 3 s of 16 kHz, 16-bit mono audio.
#define AUDIO_BYTES 96000u

// Bound on allocations per round, so an allocator that never refuses cannot spin forever. The
// smallest block the allocator hands out is a few tens of bytes, so 240 KiB of DRAM is far below
// this many blocks.
#define MAX_ALLOCS 65536u

#define MAX_HEAPS 16

static const struct {
    const char *name;
    uint32_t caps;
} CAP_SETS[] = {
    {"default", MALLOC_CAP_DEFAULT},
    {"internal_8bit", MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT},
    {"dma", MALLOC_CAP_DMA},
    {"exec", MALLOC_CAP_EXEC},
    {"rtcram", MALLOC_CAP_RTCRAM},
};

#define CAP_SET_COUNT (sizeof(CAP_SETS) / sizeof(CAP_SETS[0]))

// Used bytes per heap region, in the order `heap_caps_walk_all` first meets each region.
typedef struct {
    intptr_t start[MAX_HEAPS];
    intptr_t end[MAX_HEAPS];
    size_t used[MAX_HEAPS];
    size_t count;
} regions_t;

typedef struct {
    size_t free_before;
    size_t largest_before;
    size_t free_after;
    uint32_t allocs;
    size_t requested;
    size_t usable;
    bool exhausted;
    regions_t before;
    regions_t drained;
} round_t;

static bool add_used(walker_heap_into_t heap, walker_block_info_t block, void *user_data)
{
    regions_t *regions = (regions_t *)user_data;
    size_t i = 0;
    while (i < regions->count && regions->start[i] != heap.start) {
        i++;
    }
    if (i == regions->count) {
        if (regions->count == MAX_HEAPS) {
            return false;
        }
        regions->start[i] = heap.start;
        regions->end[i] = heap.end;
        regions->used[i] = 0;
        regions->count++;
    }
    if (block.used) {
        regions->used[i] += block.size;
    }
    return true;
}

static void walk(regions_t *regions)
{
    memset(regions, 0, sizeof(*regions));
    heap_caps_walk_all(add_used, regions);
}

// Allocates until `caps` refuses even the smallest request, chaining every block through its
// first word, then frees the chain. Nothing is printed while memory is drained: the console path
// is not allowed to be the thing that fails.
static void drain(uint32_t caps, round_t *round)
{
    memset(round, 0, sizeof(*round));
    round->free_before = heap_caps_get_free_size(caps);
    round->largest_before = heap_caps_get_largest_free_block(caps);
    walk(&round->before);

    void *chain = NULL;
    while (round->allocs < MAX_ALLOCS) {
        size_t want = heap_caps_get_largest_free_block(caps);
        if (want < sizeof(void *)) {
            want = sizeof(void *);
        }
        void *block = heap_caps_malloc(want, caps);
        while (block == NULL && want > sizeof(void *)) {
            want /= 2;
            if (want < sizeof(void *)) {
                want = sizeof(void *);
            }
            block = heap_caps_malloc(want, caps);
        }
        if (block == NULL) {
            round->exhausted = true;
            break;
        }
        *(void **)block = chain;
        chain = block;
        round->allocs++;
        round->requested += want;
        round->usable += heap_caps_get_allocated_size(block);
    }

    walk(&round->drained);
    while (chain != NULL) {
        void *next = *(void **)chain;
        heap_caps_free(chain);
        chain = next;
    }
    round->free_after = heap_caps_get_free_size(caps);
}

static bool print_round(const char *name, const round_t *round)
{
    bool ok = true;
    bool restored = round->free_after == round->free_before;
    printf("LIMIT|%s|free=%u|largest=%u|allocs=%" PRIu32 "|requested=%u|usable=%u|exhausted=%d"
           "|free_after=%u|restored=%d\n",
           name, (unsigned)round->free_before, (unsigned)round->largest_before, round->allocs,
           (unsigned)round->requested, (unsigned)round->usable, round->exhausted ? 1 : 0,
           (unsigned)round->free_after, restored ? 1 : 0);
    for (size_t i = 0; i < round->drained.count; i++) {
        size_t before = 0;
        for (size_t j = 0; j < round->before.count; j++) {
            if (round->before.start[j] == round->drained.start[i]) {
                before = round->before.used[j];
            }
        }
        size_t after = round->drained.used[i];
        // Unsigned: a region whose used bytes went down during the drain (a block some other task
        // freed) would wrap to a huge `drained`. It is printed as 0 with `shrunk=1`, and fails.
        bool shrunk = after < before;
        size_t taken = shrunk ? 0u : after - before;
        printf("LIMREG|%s|index=%u|start=0x%08" PRIxPTR "|end=0x%08" PRIxPTR "|size=%u"
               "|used_before=%u|used_drained=%u|drained=%u|shrunk=%d\n",
               name, (unsigned)i, (uintptr_t)round->drained.start[i],
               (uintptr_t)round->drained.end[i],
               (unsigned)(round->drained.end[i] - round->drained.start[i]), (unsigned)before,
               (unsigned)after, (unsigned)taken, shrunk ? 1 : 0);
        if (shrunk) {
            PROBE_FAIL("drain_region_shrunk", name);
            ok = false;
        }
    }
    if (!round->exhausted) {
        PROBE_FAIL("drain_bound", name);
        ok = false;
    }
    if (!restored) {
        PROBE_FAIL("drain_restore", name);
        ok = false;
    }
    return ok;
}

static void print_regions(void)
{
    regions_t regions;
    walk(&regions);
    for (size_t i = 0; i < regions.count; i++) {
        printf("HEAPREG|index=%u|start=0x%08" PRIxPTR "|end=0x%08" PRIxPTR "|size=%" PRIu32 "\n",
               (unsigned)i, (uintptr_t)regions.start[i], (uintptr_t)regions.end[i],
               (uint32_t)(regions.end[i] - regions.start[i]));
    }
}

// The 96000-byte record buffer, then the boundary around the largest free block.
static bool audio_limits(void)
{
    bool ok = true;
    size_t largest = heap_caps_get_largest_free_block(MALLOC_CAP_DEFAULT);
    void *audio = malloc(AUDIO_BYTES);
    printf("AUDIO|size=%u|largest=%u|ok=%d\n", (unsigned)AUDIO_BYTES, (unsigned)largest,
           audio != NULL ? 1 : 0);
    if (audio == NULL) {
        PROBE_FAIL("audio_malloc", "malloc(96000) returned NULL");
        ok = false;
    }
    free(audio);

    largest = heap_caps_get_largest_free_block(MALLOC_CAP_DEFAULT);
    void *at = malloc(largest);
    bool at_ok = at != NULL;
    free(at);
    void *above = malloc(largest + 1);
    bool above_ok = above != NULL;
    free(above);
    printf("ABOVE|largest=%u|at=%d|above=%d\n", (unsigned)largest, at_ok ? 1 : 0,
           above_ok ? 1 : 0);
    if (!at_ok) {
        PROBE_FAIL("largest_malloc", "a request equal to the largest free block failed");
        ok = false;
    }
    if (above_ok) {
        PROBE_FAIL("above_largest", "a request above the largest free block succeeded");
        ok = false;
    }
    return ok;
}

void app_main(void)
{
    PROBE_BEGIN(PROBE_NAME);
    bool ok = true;
    // All 32 bytes from the app description. `esp_app_get_elf_sha256` is not used: it stops at
    // CONFIG_APP_RETRIEVE_LEN_ELF_SHA hex digits (9 by default), too few to identify an image.
    const uint8_t *sha = esp_app_get_description()->app_elf_sha256;
    printf("IMAGE|elf_sha256=");
    for (int i = 0; i < 32; i++) {
        printf("%02x", sha[i]);
    }
    printf("\n");
    print_regions();

    static round_t round;
    for (size_t i = 0; i < CAP_SET_COUNT; i++) {
        drain(CAP_SETS[i].caps, &round);
        ok &= print_round(CAP_SETS[i].name, &round);
    }

    ok &= audio_limits();

    if (!heap_caps_check_integrity_all(true)) {
        PROBE_FAIL("heap_integrity", "heap_caps_check_integrity_all failed");
        ok = false;
    }
    PROBE_END(PROBE_NAME, ok ? "ok" : "fail");
}
