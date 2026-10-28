/*
 * ESP-IDF/FreeRTOS counterpart of the p4os `bench` shell command
 * (kernel/src/bench.rs). Same tests, parameters and output format, built
 * with FreeRTOS primitives on a single core, so the two systems can be
 * compared on one board.
 *
 * Times come from mcycle (one CPU cycle) except the alarm-to-handler
 * latency, measured against the systimer counter the alarm compares with
 * (62.5 ns resolution). Systimer comparator 1 is free with
 * CONFIG_FREERTOS_UNICORE (IDF uses comparator 0 for the tick, 2 for
 * esp_timer).
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "esp_attr.h"
#include "esp_heap_caps.h"
#include "esp_intr_alloc.h"
#include "esp_memory_utils.h"
#include "esp_system.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "soc/interrupts.h"
#include "soc/soc.h"
#include "soc/systimer_reg.h"

#define RUNS 5
/* Above every system task except esp_timer (22), which stays idle. */
#define PRIORITY 20
#define STACK (8 * 1024)
#define CORE 0

#define PINGPONG_ROUNDS 10000u
#define YIELDS 10000u
#define MUTEX_OPS 100000u
#define IRQ_SAMPLES 1000u
#define TICKS_PER_US 16u
#define IRQ_LEAD_TICKS (50u * TICKS_PER_US)
#define HEAP_OPS 10000u
#define MIB (1u << 20)
#define SRAM_COPY (128u * 1024)
#define CRC_BUF (64u * 1024)
#define INT_ITERS 1000000u
#define SEED 0x12345678u

/* Keep the compiler from eliding work whose result is otherwise unused. */
#define BLACK_BOX(p) __asm__ volatile("" : : "r"(p) : "memory")

static uint32_t cpu_mhz = 360;
static SemaphoreHandle_t helper_done, ping, pong, isr_sem, bench_mutex;
static TaskHandle_t runner_task;
static volatile uint32_t mutex_counter;

enum wake_mode { WAKE_NONE, WAKE_SEMAPHORE, WAKE_NOTIFY };
static volatile uint32_t wake_mode = WAKE_NONE;
static volatile uint32_t isr_cycles, isr_ticks, spin_cycles, entry_cycles, fired;

static inline __attribute__((always_inline)) uint32_t cycles(void)
{
    uint32_t v;
    __asm__ volatile("csrr %0, mcycle" : "=r"(v));
    return v;
}

static float cycles_to_ns(uint32_t c) { return (float)c * 1000.0f / (float)cpu_mhz; }
static float ticks_to_ns(uint32_t t) { return (float)t * 1000.0f / (float)TICKS_PER_US; }
static float mb_per_s(uint32_t bytes, uint32_t c) { return (float)bytes * (float)cpu_mhz / (float)(c ? c : 1); }

/* Same register sequence as p4os `systimer::now_ticks` (unit 0). */
static inline __attribute__((always_inline)) uint64_t systimer_now(void)
{
    REG_WRITE(SYSTIMER_UNIT0_OP_REG, SYSTIMER_TIMER_UNIT0_UPDATE);
    while (!(REG_READ(SYSTIMER_UNIT0_OP_REG) & SYSTIMER_TIMER_UNIT0_VALUE_VALID)) {
    }
    uint64_t hi = REG_READ(SYSTIMER_UNIT0_VALUE_HI_REG);
    uint64_t lo = REG_READ(SYSTIMER_UNIT0_VALUE_LO_REG);
    return (hi << 32) | lo;
}

static uint32_t measure_cpu_mhz(void)
{
    int64_t t0 = esp_timer_get_time();
    uint32_t c0 = cycles();
    while (esp_timer_get_time() - t0 < 10000) {
    }
    uint32_t c1 = cycles();
    int64_t us = esp_timer_get_time() - t0;
    return (uint32_t)((c1 - c0) / us);
}

static int cmp_float(const void *a, const void *b)
{
    float x = *(const float *)a, y = *(const float *)b;
    return (x > y) - (x < y);
}

static void row(const char *name, const char *unit, float v[RUNS])
{
    qsort(v, RUNS, sizeof(float), cmp_float);
    printf("%-40s %-9s %10.1f %10.1f %10.1f\n", name, unit, v[RUNS / 2], v[0], v[RUNS - 1]);
}

static void spawn(TaskFunction_t f, const char *name)
{
    if (xTaskCreatePinnedToCore(f, name, STACK, NULL, PRIORITY, NULL, CORE) != pdPASS) {
        printf("bench: cannot create task %s\n", name);
        abort();
    }
}

/* --- scheduler and synchronisation ------------------------------------- */

static void pong_task(void *arg)
{
    for (uint32_t i = 0; i < PINGPONG_ROUNDS; i++) {
        xSemaphoreTake(ping, portMAX_DELAY);
        xSemaphoreGive(pong);
    }
    xSemaphoreGive(helper_done);
    vTaskDelete(NULL);
}

/* Each round trip is two context switches plus one give and one take on
 * each side; reported per switch. */
static float pingpong(void)
{
    spawn(pong_task, "bench-pong");
    uint32_t t0 = cycles();
    for (uint32_t i = 0; i < PINGPONG_ROUNDS; i++) {
        xSemaphoreGive(ping);
        xSemaphoreTake(pong, portMAX_DELAY);
    }
    uint32_t dt = cycles() - t0;
    xSemaphoreTake(helper_done, portMAX_DELAY);
    return cycles_to_ns(dt) / (2 * PINGPONG_ROUNDS);
}

static void yield_task(void *arg)
{
    for (uint32_t i = 0; i < YIELDS; i++) {
        taskYIELD();
    }
    xSemaphoreGive(helper_done);
    vTaskDelete(NULL);
}

/* Two equal-priority tasks yielding to each other; every yield switches. */
static float yield_pair(void)
{
    spawn(yield_task, "bench-yield");
    uint32_t t0 = cycles();
    for (uint32_t i = 0; i < YIELDS; i++) {
        taskYIELD();
    }
    uint32_t dt = cycles() - t0;
    xSemaphoreTake(helper_done, portMAX_DELAY);
    return cycles_to_ns(dt) / (2 * YIELDS);
}

static float mutex_pair(void)
{
    uint32_t t0 = cycles();
    for (uint32_t i = 0; i < MUTEX_OPS; i++) {
        xSemaphoreTake(bench_mutex, portMAX_DELAY);
        mutex_counter++;
        xSemaphoreGive(bench_mutex);
    }
    return cycles_to_ns(cycles() - t0) / MUTEX_OPS;
}

/* --- interrupt latency ---------------------------------------------------- */

static void IRAM_ATTR on_alarm(void *arg)
{
    uint32_t c = cycles();
    uint32_t t = (uint32_t)systimer_now();
    entry_cycles = c - spin_cycles;
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    isr_cycles = c;
    isr_ticks = t;
    fired = 1;
    BaseType_t woken = pdFALSE;
    if (wake_mode == WAKE_SEMAPHORE) {
        xSemaphoreGiveFromISR(isr_sem, &woken);
    } else if (wake_mode == WAKE_NOTIFY) {
        vTaskNotifyGiveFromISR(runner_task, &woken);
    }
    portYIELD_FROM_ISR(woken);
}

static intr_handle_t alarm_handle;
static portMUX_TYPE alarm_lock = portMUX_INITIALIZER_UNLOCKED;

static void alarm_init(void)
{
    portENTER_CRITICAL(&alarm_lock);
    REG_WRITE(SYSTIMER_TARGET1_CONF_REG, 0); /* one-shot, compares with unit 0 */
    REG_SET_BIT(SYSTIMER_CONF_REG, SYSTIMER_TARGET1_WORK_EN);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    REG_SET_BIT(SYSTIMER_INT_ENA_REG, SYSTIMER_TARGET1_INT_ENA);
    portEXIT_CRITICAL(&alarm_lock);
    esp_err_t err = esp_intr_alloc(ETS_SYSTIMER_TARGET1_INTR_SOURCE, ESP_INTR_FLAG_LEVEL3 | ESP_INTR_FLAG_IRAM,
                                   on_alarm, NULL, &alarm_handle);
    if (err != ESP_OK) {
        printf("bench: esp_intr_alloc failed: %s\n", esp_err_to_name(err));
        abort();
    }
}

static void alarm_stop(void)
{
    portENTER_CRITICAL(&alarm_lock);
    REG_CLR_BIT(SYSTIMER_INT_ENA_REG, SYSTIMER_TARGET1_INT_ENA);
    REG_CLR_BIT(SYSTIMER_CONF_REG, SYSTIMER_TARGET1_WORK_EN);
    REG_WRITE(SYSTIMER_INT_CLR_REG, SYSTIMER_TARGET1_INT_CLR);
    portEXIT_CRITICAL(&alarm_lock);
    esp_intr_free(alarm_handle);
}

/* Arm comparator 1 IRQ_LEAD_TICKS ahead and return the deadline. */
static uint32_t alarm_arm(void)
{
    uint64_t deadline = systimer_now() + IRQ_LEAD_TICKS;
    REG_WRITE(SYSTIMER_TARGET1_HI_REG, (uint32_t)(deadline >> 32) & 0xFFFFF);
    REG_WRITE(SYSTIMER_TARGET1_LO_REG, (uint32_t)deadline);
    REG_WRITE(SYSTIMER_COMP1_LOAD_REG, 1);
    return (uint32_t)deadline;
}

/* The runner blocks (semaphore or task notification) until the alarm
 * handler wakes it. out = {avg irq latency, max irq latency, avg wake,
 * max wake} in ns. */
static void irq_wake_run(enum wake_mode mode, float out[4])
{
    uint64_t lat_sum = 0, wake_sum = 0;
    uint32_t lat_max = 0, wake_max = 0;
    wake_mode = mode;
    for (uint32_t i = 0; i < IRQ_SAMPLES; i++) {
        uint32_t deadline = alarm_arm();
        if (mode == WAKE_SEMAPHORE) {
            xSemaphoreTake(isr_sem, portMAX_DELAY);
        } else {
            ulTaskNotifyTake(pdTRUE, portMAX_DELAY);
        }
        uint32_t woke = cycles();
        uint32_t lat = isr_ticks - deadline;
        uint32_t wake = woke - isr_cycles;
        lat_sum += lat;
        wake_sum += wake;
        lat_max = lat > lat_max ? lat : lat_max;
        wake_max = wake > wake_max ? wake : wake_max;
    }
    wake_mode = WAKE_NONE;
    out[0] = ticks_to_ns((uint32_t)lat_sum) / IRQ_SAMPLES;
    out[1] = ticks_to_ns(lat_max);
    out[2] = cycles_to_ns((uint32_t)wake_sum) / IRQ_SAMPLES;
    out[3] = cycles_to_ns(wake_max);
}

/* The runner spins storing mcycle; the handler subtracts the last stored
 * value from its own first mcycle read (trap entry cost).
 * out = {avg irq latency (systimer), avg entry (mcycle)} in ns. */
static void irq_busy_run(float out[2])
{
    uint64_t lat_sum = 0, entry_sum = 0;
    for (uint32_t i = 0; i < IRQ_SAMPLES; i++) {
        fired = 0;
        uint32_t deadline = alarm_arm();
        while (!fired) {
            spin_cycles = cycles();
        }
        lat_sum += isr_ticks - deadline;
        entry_sum += entry_cycles;
    }
    out[0] = ticks_to_ns((uint32_t)lat_sum) / IRQ_SAMPLES;
    out[1] = cycles_to_ns((uint32_t)entry_sum) / IRQ_SAMPLES;
}

/* --- heap -------------------------------------------------------------------- */

static void heap_row(const char *name, size_t size, uint32_t caps, bool want_psram)
{
    void *probe = heap_caps_malloc(size, caps);
    if (!probe) {
        printf("%-40s allocation failed\n", name);
        return;
    }
    bool in_psram = esp_ptr_external_ram(probe);
    heap_caps_free(probe);
    if (in_psram != want_psram) {
        printf("%-40s landed in the wrong memory (%p)\n", name, probe);
        return;
    }
    float v[RUNS];
    for (int r = 0; r < RUNS; r++) {
        uint32_t t0 = cycles();
        for (uint32_t i = 0; i < HEAP_OPS; i++) {
            void *p = heap_caps_malloc(size, caps);
            BLACK_BOX(p);
            heap_caps_free(p);
        }
        v[r] = cycles_to_ns(cycles() - t0) / HEAP_OPS;
    }
    row(name, "ns", v);
}

/* --- memory and CPU kernels (OS independent) ------------------------------- */

static uint32_t xorshift(uint32_t *x)
{
    *x ^= *x << 13;
    *x ^= *x >> 17;
    *x ^= *x << 5;
    return *x;
}

static uint8_t *buf_new(size_t len, uint32_t caps)
{
    uint8_t *b = heap_caps_aligned_alloc(64, len, caps);
    if (b) {
        uint32_t x = SEED;
        for (size_t i = 0; i < len; i++) {
            b[i] = (uint8_t)xorshift(&x);
        }
    }
    return b;
}

/* Copy MIB bytes in total from src to dst (repeating the buffer when it is
 * smaller than 1 MiB). Returns MB/s. */
static float copy_mib(const uint8_t *src, uint8_t *dst, size_t len)
{
    uint32_t passes = MIB / len;
    uint32_t t0 = cycles();
    for (uint32_t i = 0; i < passes; i++) {
        BLACK_BOX(src);
        memcpy(dst, src, len);
        BLACK_BOX(dst);
    }
    return mb_per_s(passes * len, cycles() - t0);
}

static void copy_row(const char *name, size_t len, uint32_t caps)
{
    uint8_t *src = buf_new(len, caps), *dst = buf_new(len, caps);
    if (src && dst) {
        float v[RUNS];
        for (int r = 0; r < RUNS; r++) {
            v[r] = copy_mib(src, dst, len);
        }
        row(name, "MB/s", v);
    } else {
        printf("%s: not enough memory for 2 x %u KiB\n", name, (unsigned)(len / 1024));
    }
    heap_caps_free(src);
    heap_caps_free(dst);
}

static uint32_t crc_table[256];

static void crc32_table_init(void)
{
    for (uint32_t n = 0; n < 256; n++) {
        uint32_t c = n;
        for (int k = 0; k < 8; k++) {
            c = (c & 1) ? 0xEDB88320u ^ (c >> 1) : c >> 1;
        }
        crc_table[n] = c;
    }
}

/* Reflected CRC-32 (IEEE), raw state in and out (no pre/post inversion). */
static uint32_t crc32_update(uint32_t c, const uint8_t *data, size_t len)
{
    for (size_t i = 0; i < len; i++) {
        c = crc_table[(c ^ data[i]) & 0xFF] ^ (c >> 8);
    }
    return c;
}

/* Fixed integer mix: xorshift, multiply, rotate, divide and a data
 * dependent branch, so no single instruction class dominates. */
static uint32_t int_kernel(uint32_t n, uint32_t seed)
{
    uint32_t x = seed, acc = 0;
    for (uint32_t i = 0; i < n; i++) {
        xorshift(&x);
        acc = ((acc << 5) | (acc >> 27)) ^ (x * 0x9E3779B9u);
        acc += x / ((i & 0xFF) + 1);
        if (x & 1) {
            acc ^= i;
        }
    }
    return acc;
}

static void cpu_rows(void)
{
    crc32_table_init();
    uint32_t crc = 0, sum = 0;
    float v[RUNS];
    uint8_t *buf = buf_new(CRC_BUF, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    if (buf) {
        for (int r = 0; r < RUNS; r++) {
            uint32_t t0 = cycles();
            uint32_t c = ~0u;
            for (uint32_t i = 0; i < MIB / CRC_BUF; i++) {
                BLACK_BOX(buf);
                c = crc32_update(c, buf, CRC_BUF);
            }
            uint32_t dt = cycles() - t0;
            crc = ~c;
            v[r] = mb_per_s(MIB, dt);
        }
        row("crc32 1 MiB (64 KiB sram x16)", "MB/s", v);
        heap_caps_free(buf);
    } else {
        printf("crc32: not enough SRAM\n");
    }
    for (int r = 0; r < RUNS; r++) {
        volatile uint32_t n = INT_ITERS, seed = SEED;
        uint32_t t0 = cycles();
        sum = int_kernel(n, seed);
        v[r] = cycles_to_ns(cycles() - t0) / INT_ITERS;
    }
    row("integer kernel (per iteration)", "ns", v);
    printf("checks: crc32=0x%08lx int=0x%08lx\n", (unsigned long)crc, (unsigned long)sum);
}

/* --- runner ------------------------------------------------------------------ */

static void runner(void *arg)
{
    float v[RUNS];
    /* Let boot logging drain and app_main return before measuring. */
    vTaskDelay(pdMS_TO_TICKS(1000));
    cpu_mhz = measure_cpu_mhz();
    printf("bench: freertos (esp-idf %s), cpu %lu MHz, %d runs per test, median/min/max over runs\n",
           esp_get_idf_version(), (unsigned long)cpu_mhz, RUNS);
    printf("%-40s %-9s %10s %10s %10s\n", "test", "unit", "median", "min", "max");

    for (int r = 0; r < RUNS; r++) v[r] = pingpong();
    row("ctx switch (sem ping-pong)", "ns", v);
    for (int r = 0; r < RUNS; r++) v[r] = yield_pair();
    row("yield (2 equal-prio threads)", "ns", v);
    for (int r = 0; r < RUNS; r++) v[r] = mutex_pair();
    row("mutex lock+unlock (uncontended)", "ns", v);

    alarm_init();
    float sem[RUNS][4], ntf[RUNS][4], busy[RUNS][2];
    for (int r = 0; r < RUNS; r++) {
        irq_wake_run(WAKE_SEMAPHORE, sem[r]);
        irq_wake_run(WAKE_NOTIFY, ntf[r]);
        irq_busy_run(busy[r]);
    }
    alarm_stop();
#define COLUMN(src, i) do { for (int r = 0; r < RUNS; r++) v[r] = (src)[r][i]; } while (0)
    COLUMN(sem, 0); row("irq latency avg (thread blocked)", "ns", v);
    COLUMN(sem, 1); row("irq latency max (thread blocked)", "ns", v);
    COLUMN(busy, 0); row("irq latency avg (cpu busy)", "ns", v);
    COLUMN(busy, 1); row("irq entry avg (cpu busy, mcycle)", "ns", v);
    COLUMN(sem, 2); row("isr->thread wake avg (semaphore)", "ns", v);
    COLUMN(sem, 3); row("isr->thread wake max (semaphore)", "ns", v);
    COLUMN(ntf, 2); row("isr->thread wake avg (task notify)", "ns", v);
    COLUMN(ntf, 3); row("isr->thread wake max (task notify)", "ns", v);
#undef COLUMN

    heap_row("heap alloc+free 32 B (sram)", 32, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT, false);
    heap_row("heap alloc+free 1 KiB (sram)", 1024, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT, false);
    heap_row("heap alloc+free 256 KiB (psram)", 256 * 1024, MALLOC_CAP_SPIRAM, true);

    copy_row("memcpy sram->sram (128 KiB x8)", SRAM_COPY, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT);
    copy_row("memcpy psram->psram (1 MiB)", MIB, MALLOC_CAP_SPIRAM);
    cpu_rows();
    printf("free heap: internal %u, psram %u\n", (unsigned)heap_caps_get_free_size(MALLOC_CAP_INTERNAL),
           (unsigned)heap_caps_get_free_size(MALLOC_CAP_SPIRAM));
    printf("bench: done\n");
    vTaskDelete(NULL);
}

void app_main(void)
{
    helper_done = xSemaphoreCreateBinary();
    ping = xSemaphoreCreateBinary();
    pong = xSemaphoreCreateBinary();
    isr_sem = xSemaphoreCreateBinary();
    bench_mutex = xSemaphoreCreateMutex();
    if (!helper_done || !ping || !pong || !isr_sem || !bench_mutex) {
        printf("bench: cannot create semaphores\n");
        abort();
    }
    if (xTaskCreatePinnedToCore(runner, "bench", STACK, NULL, PRIORITY, &runner_task, CORE) != pdPASS) {
        printf("bench: cannot create runner task\n");
        abort();
    }
}
