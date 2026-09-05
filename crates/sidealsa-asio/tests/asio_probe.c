/* SPDX-License-Identifier: GPL-3.0-or-later */

#define WIN32_LEAN_AND_MEAN
#define COBJMACROS

#include <objbase.h>
#include <windows.h>

#include <errno.h>
#include <math.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef SIDEALSA_ASIO_LOOPBACK_DEFAULT
#define SIDEALSA_ASIO_LOOPBACK_DEFAULT 0
#endif

#ifndef SIDEALSA_ASIO_LIFECYCLE_DEFAULT
#define SIDEALSA_ASIO_LIFECYCLE_DEFAULT 0
#endif

#ifndef SCHED_RESET_ON_FORK
#define SCHED_RESET_ON_FORK 0x40000000
#endif

typedef struct
{
    ULONG hi;
    ULONG lo;
} AsioInt64;

typedef struct
{
    LONG reserved[4];
    double speed;
    AsioInt64 time_stamp;
    AsioInt64 sample_position;
    double sample_rate;
    ULONG flags;
    char reserved_2[12];
    double speed_for_time_code;
    AsioInt64 time_code;
    ULONG time_code_flags;
    char reserved_3[64];
} AsioTimeInfo;

typedef struct
{
    LONG  is_input_type;
    LONG  channel_number;
    void *buffers[2];
} AsioBufferInfo;

typedef struct
{
    LONG channel;
    LONG is_input;
    LONG is_active;
    LONG channel_group;
    LONG sample_type;
    char name[32];
} AsioChannelInfo;

typedef struct
{
    LONG index;
    LONG associated_channel;
    LONG associated_group;
    LONG is_current_source;
    char name[32];
} AsioClockSource;

typedef struct
{
    void(CALLBACK *buffer_switch)(LONG, LONG);
    void (CALLBACK *sample_rate_changed)(double);
    LONG(CALLBACK *asio_message)(LONG, LONG, void *, double *);
    void *(CALLBACK *buffer_switch_time_info)(void *, LONG, LONG);
} AsioCallbacks;

typedef struct SideAlsaAsio SideAlsaAsio;
typedef struct
{
    HRESULT(WINAPI *QueryInterface)(SideAlsaAsio *, REFIID, void **);
    ULONG(WINAPI *AddRef)(SideAlsaAsio *);
    ULONG(WINAPI *Release)(SideAlsaAsio *);
    LONG(WINAPI *Init)(SideAlsaAsio *, void *);
    void(WINAPI *GetDriverName)(SideAlsaAsio *, char *);
    LONG(WINAPI *GetDriverVersion)(SideAlsaAsio *);
    void(WINAPI *GetErrorMessage)(SideAlsaAsio *, char *);
    LONG(WINAPI *Start)(SideAlsaAsio *);
    LONG(WINAPI *Stop)(SideAlsaAsio *);
    LONG(WINAPI *GetChannels)(SideAlsaAsio *, LONG *, LONG *);
    LONG(WINAPI *GetLatencies)(SideAlsaAsio *, LONG *, LONG *);
    LONG(WINAPI *GetBufferSize)(SideAlsaAsio *, LONG *, LONG *, LONG *, LONG *);
    LONG(WINAPI *CanSampleRate)(SideAlsaAsio *, double);
    LONG(WINAPI *GetSampleRate)(SideAlsaAsio *, double *);
    LONG(WINAPI *SetSampleRate)(SideAlsaAsio *, double);
    LONG(WINAPI *GetClockSources)(SideAlsaAsio *, void *, LONG *);
    LONG(WINAPI *SetClockSource)(SideAlsaAsio *, LONG);
    LONG(WINAPI *GetSamplePosition)(SideAlsaAsio *, AsioInt64 *, AsioInt64 *);
    LONG(WINAPI *GetChannelInfo)(SideAlsaAsio *, AsioChannelInfo *);
    LONG(WINAPI *CreateBuffers)(SideAlsaAsio *, AsioBufferInfo *, LONG, LONG, AsioCallbacks *);
    LONG(WINAPI *DisposeBuffers)(SideAlsaAsio *);
    LONG(WINAPI *ControlPanel)(SideAlsaAsio *);
    LONG(WINAPI *Future)(SideAlsaAsio *, LONG, void *);
    LONG(WINAPI *OutputReady)(SideAlsaAsio *);
} SideAlsaAsioVtbl;

struct SideAlsaAsio
{
    const SideAlsaAsioVtbl *lpVtbl;
};

static const CLSID CLSID_SideAlsaAsio
        = { 0x8c4d6a10, 0x5a7d, 0x4cc2, { 0xae, 0x13, 0x7d, 0x9e, 0x3e, 0x2a, 0x1b, 0x40 } };
static LONG cycles;
static LONG first_index = -1;
static LONG callback_thread;
static LONG callback_thread_mismatch;
static LONG invalid_callback_stack;
static LONG start_thread;
static LONG loopback_enabled;
static AsioBufferInfo *active_buffers;
static uint64_t loopback_frame;
static uint64_t callback_block_start;
static uint64_t next_pulse_frame;
static uint64_t pending_pulse_frame;
static uint64_t loopback_total;
static LONG pending_pulse;
static LONG loopback_measurements;
static LONG loopback_lost;
static LONG loopback_emitted;
static LONG loopback_emit;
static LONG callback_block_start_valid;
static LONG loopback_min;
static LONG loopback_max;
static LONG last_callback_index;
static LONG callback_index_errors;
static LONG sample_position_seen;
static LONG sample_position_errors;
static uint64_t last_sample_position;
static SideAlsaAsio *active_asio;
static HANDLE callback_entered_event;
static HANDLE callback_release_event;
static HANDLE callback_finished_event;
static HANDLE callback_stop_event;
static LONG block_callback_once;
static LONG callback_stop_once;
static LONG callback_stop_result;
static LONG callback_wait_errors;

#define LOOPBACK_FIRST_PULSE_FRAME 65
#define LOOPBACK_INTERVAL_FRAMES 4097
#define MAX_STRESS_THREADS 64
#define STRESS_CPU_BATCH 65536
#define STRESS_GATE_CHUNK 1024
#define MAX_SINE_VOICES 4096
#define SINE_STAGE_COUNT 3
#define SINE_TWO_PI 6.28318530717958647692
#define SINE_PEAK 0.03125

static struct
{
    double phase;
    double step;
} sine_oscillators[MAX_SINE_VOICES];

static struct
{
    uint64_t callbacks;
    LONG64 total_ticks;
    LONG64 max_ticks;
    uint64_t period_overruns;
    uint64_t voice_samples;
    uint64_t loopback_count;
    LONG loopback_min;
    LONG loopback_max;
    uint64_t first_sample_position;
    uint64_t first_mono_ns;
    LONG64 worker_begin;
    LONG64 worker_end;
} sine_stats[SINE_STAGE_COUNT];

static DWORD sine_voices;
static uint64_t sine_total_frames;
static LONG sine_stage = -1;
static LONG pending_pulse_stage = -1;
static volatile double sine_prewarm_sink;
static volatile LONG stress_active;

typedef struct
{
    HANDLE handle;
    BYTE *memory;
    SIZE_T memory_size;
    uint64_t state;
} StressWorker;

static StressWorker stress_workers[MAX_STRESS_THREADS];
static HANDLE stress_start_event;
static BYTE *stress_memory;
static DWORD stress_thread_count;
static DWORD stress_threads_started;
static SIZE_T stress_memory_bytes;
static volatile LONG stress_stop_requested;
static volatile LONG64 stress_passes;
static volatile LONG64 stress_work_units;
static volatile LONG64 stress_checksum;
static volatile uint64_t callback_work_sink;
static int benchmark_enabled;
static int benchmark_running;
static DWORD benchmark_heartbeat_ms = 10;
static DWORD benchmark_rt_priority;
static LARGE_INTEGER benchmark_frequency;
static LARGE_INTEGER benchmark_started;
static LONG64 callback_work_ticks;
static LONG64 callback_period_ticks;
static volatile LONG64 callback_timed_cycles;
static volatile LONG64 callback_total_ticks;
static volatile LONG64 callback_max_ticks;
static volatile LONG callback_period_overruns;
static volatile LONG64 host_heartbeats;
static volatile LONG64 host_heartbeat_max_gap_ticks;
static volatile LONG64 host_heartbeat_max_late_ticks;
static LONG callback_sched_policy = INT32_MIN;
static LONG callback_sched_priority = INT32_MIN;
static LONG callback_sched_reset_on_fork;
static LONG callback_sched_set_attempted;
static LONG callback_sched_set_error;

static int
prepare_sine(DWORD run_ms)
{
    struct timespec now;

    if (sine_voices == 0 || sine_voices > MAX_SINE_VOICES || run_ms < 1000)
        return 0;
    sine_total_frames = (uint64_t)run_ms * 48;
    memset(sine_oscillators, 0, sizeof(sine_oscillators));
    for (DWORD voice = 0; voice < sine_voices; ++voice)
    {
        double frequency = 220.0;

        if (sine_voices > 1)
            frequency += 1780.0 * voice / (sine_voices - 1);
        sine_oscillators[voice].step = SINE_TWO_PI * frequency / 48000.0;
    }
    /* Prewarm the full phase range even for one voice, before any callback can run. */
    for (LONG sample = 0; sample < 64; ++sample)
        sine_prewarm_sink = sin(SINE_TWO_PI * sample / 64.0 + sine_oscillators[0].step);
    return clock_gettime(CLOCK_MONOTONIC, &now) == 0;
}

static void
reset_sine(void)
{
    /* Only called before Start, with the previous callback fully stopped. */
    InterlockedExchange(&stress_active, 0);
    for (DWORD voice = 0; voice < sine_voices; ++voice)
        sine_oscillators[voice].phase = 0.0;
    memset(sine_stats, 0, sizeof(sine_stats));
    for (LONG stage = 0; stage < SINE_STAGE_COUNT; ++stage)
        sine_stats[stage].loopback_min = INT32_MAX;
    sine_stage = -1;
    pending_pulse_stage = -1;
}

static void
select_sine_stage(uint64_t block_start)
{
    uint64_t elapsed = sine_stage < 0 ? 0 : block_start - sine_stats[0].first_sample_position;
    LONG stage = elapsed < sine_total_frames / 4
                   ? 0
                   : elapsed < sine_total_frames * 3 / 4 ? 1 : 2;

    if (stage != sine_stage)
    {
        struct timespec now;
        LONG64 work_units = InterlockedCompareExchange64(&stress_work_units, 0, 0);

        /* Transitions use sample time, rounded up to a callback boundary, never host IPC. */
        if (sine_stage >= 0)
            sine_stats[sine_stage].worker_end = work_units;
        sine_stats[stage].worker_begin = work_units;
        sine_stats[stage].worker_end = work_units;
        sine_stage = stage;
        sine_stats[stage].first_sample_position = block_start;
        sine_stats[stage].first_mono_ns = clock_gettime(CLOCK_MONOTONIC, &now) == 0
                                          ? (uint64_t)now.tv_sec * UINT64_C(1000000000)
                                                + (uint64_t)now.tv_nsec
                                          : 0;
        InterlockedExchange(&stress_active, stage == 1);
    }
}

static void
process_sine(LONG index)
{
    DWORD voices;
    double gain;
    float *output;

    if (sine_voices == 0)
        return;
    voices = sine_stage == 1 ? sine_voices : 1;
    gain = SINE_PEAK / voices;
    output = active_buffers[10].buffers[index];
    for (LONG frame = 0; frame < 64; ++frame)
    {
        double sample = 0.0;

        for (DWORD voice = 0; voice < voices; ++voice)
        {
            double phase = sine_oscillators[voice].phase;

            sample += sin(phase);
            phase += sine_oscillators[voice].step;
            if (phase >= SINE_TWO_PI)
                phase -= SINE_TWO_PI;
            sine_oscillators[voice].phase = phase;
        }
        output[frame] += (float)(sample * gain);
    }
    for (LONG channel = 11; channel < 18; ++channel)
        memset(active_buffers[channel].buffers[index], 0, 64 * sizeof(float));
    sine_stats[sine_stage].voice_samples += (uint64_t)voices * 64;
}

static int
finish_sine_stages(void)
{
    /* Only after Stop has quiesced the callback; workers keep their global counters. */
    InterlockedExchange(&stress_active, 0);
    if (sine_stage >= 0)
        sine_stats[sine_stage].worker_end = InterlockedCompareExchange64(&stress_work_units, 0, 0);
    for (LONG stage = 0; stage < SINE_STAGE_COUNT; ++stage)
    {
        if (sine_stats[stage].callbacks == 0 || sine_stats[stage].loopback_count == 0
            || sine_stats[stage].period_overruns != 0 || sine_stats[stage].first_mono_ns == 0
            || sine_stats[stage].worker_end < sine_stats[stage].worker_begin
            || (stage == 1 && stress_thread_count > 0 && sine_voices > 0
                && sine_stats[stage].worker_end == sine_stats[stage].worker_begin))
            return 0;
    }
    return 1;
}

static int
print_sine_stages(const char *leg)
{
    static const char *const names[SINE_STAGE_COUNT] = { "warm", "loaded", "cool" };
    int success = finish_sine_stages();

    for (LONG stage = 0; stage < SINE_STAGE_COUNT; ++stage)
    {
        double ticks_per_us = (double)benchmark_frequency.QuadPart / 1000000.0;

        fprintf(stderr,
                "[asio-probe] %s sine stage: stage=%s voices=%lu count=%llu mean_us=%.3f max_us=%.3f period_overruns=%llu voice_samples=%llu loopback_count=%llu loopback_min=%ld loopback_max=%ld first_sample_position=%llu first_mono_ns=%llu worker_units=%llu\n",
                leg, names[stage], (unsigned long)(stage == 1 ? sine_voices : 1),
                (unsigned long long)sine_stats[stage].callbacks,
                sine_stats[stage].callbacks == 0
                  ? 0.0
                  : (double)sine_stats[stage].total_ticks
                        / ((double)sine_stats[stage].callbacks * ticks_per_us),
                (double)sine_stats[stage].max_ticks / ticks_per_us,
                (unsigned long long)sine_stats[stage].period_overruns,
                (unsigned long long)sine_stats[stage].voice_samples,
                (unsigned long long)sine_stats[stage].loopback_count,
                (long)(sine_stats[stage].loopback_count == 0 ? 0 : sine_stats[stage].loopback_min),
                (long)sine_stats[stage].loopback_max,
                (unsigned long long)sine_stats[stage].first_sample_position,
                (unsigned long long)sine_stats[stage].first_mono_ns,
                (unsigned long long)(sine_stats[stage].worker_end - sine_stats[stage].worker_begin));
    }
    return success;
}

static int
read_environment_u32(const char *name, DWORD maximum, DWORD *value)
{
    char text[32];
    char *end = NULL;
    DWORD length = GetEnvironmentVariableA(name, text, sizeof(text));
    unsigned long parsed;

    if (length == 0)
        return 1;
    if (length >= sizeof(text))
    {
        fprintf(stderr, "[asio-probe] %s is too long\n", name);
        return 0;
    }
    parsed = strtoul(text, &end, 10);
    if (!text[0] || !end || *end || parsed > maximum)
    {
        fprintf(stderr, "[asio-probe] %s must be between 0 and %lu\n",
                name, (unsigned long)maximum);
        return 0;
    }
    *value = (DWORD)parsed;
    return 1;
}

static void
update_max_ticks(volatile LONG64 *target, LONG64 value)
{
    LONG64 current = InterlockedCompareExchange64(target, 0, 0);

    while (value > current)
    {
        LONG64 previous = InterlockedCompareExchange64(target, value, current);
        if (previous == current)
            break;
        current = previous;
    }
}

static DWORD WINAPI
stress_worker_main(void *opaque)
{
    StressWorker *worker = opaque;
    uint64_t state = worker->state;

    if (WaitForSingleObject(stress_start_event, INFINITE) != WAIT_OBJECT_0)
        return 1;
    while (!InterlockedCompareExchange(&stress_stop_requested, 0, 0))
    {
        LONG64 work_units = 0;

        if (sine_voices > 0 && !InterlockedCompareExchange(&stress_active, 0, 0))
        {
            Sleep(1);
            continue;
        }
        if (worker->memory_size > 0)
        {
            for (SIZE_T offset = 0; offset + sizeof(uint64_t) <= worker->memory_size;
                 offset += 64)
            {
                volatile uint64_t *slot = (volatile uint64_t *)(worker->memory + offset);

                /* At most 1024 cache lines (64 KiB) of work between gate checks. */
                if (sine_voices > 0 && work_units % STRESS_GATE_CHUNK == 0
                    && (!InterlockedCompareExchange(&stress_active, 0, 0)
                        || InterlockedCompareExchange(&stress_stop_requested, 0, 0)))
                    break;
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state += *slot + (uint64_t)offset;
                *slot = state;
                ++work_units;
            }
        }
        else
        {
            for (work_units = 0; work_units < STRESS_CPU_BATCH; ++work_units)
            {
                if (sine_voices > 0 && work_units % STRESS_GATE_CHUNK == 0
                    && (!InterlockedCompareExchange(&stress_active, 0, 0)
                        || InterlockedCompareExchange(&stress_stop_requested, 0, 0)))
                    break;
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
            }
        }
        if (sine_voices == 0 || work_units > 0)
        {
            InterlockedIncrement64(&stress_passes);
            InterlockedExchangeAdd64(&stress_work_units, work_units);
        }
    }
    worker->state = state;
    InterlockedExchangeAdd64(&stress_checksum, (LONG64)state);
    return 0;
}

static int
prepare_stress_workers(void)
{
    SIZE_T chunk_size = 0;

    if (stress_thread_count == 0)
        return 1;
    stress_start_event = CreateEventA(NULL, TRUE, FALSE, NULL);
    if (!stress_start_event)
        return 0;
    if (stress_memory_bytes > 0)
    {
        stress_memory = VirtualAlloc(NULL, stress_memory_bytes,
                                     MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if (!stress_memory)
            return 0;
        memset(stress_memory, 0xa5, stress_memory_bytes);
        chunk_size = stress_memory_bytes / stress_thread_count;
        chunk_size -= chunk_size % 64;
    }
    for (DWORD index = 0; index < stress_thread_count; ++index)
    {
        StressWorker *worker = &stress_workers[index];
        SIZE_T offset = chunk_size * index;

        memset(worker, 0, sizeof(*worker));
        worker->state = UINT64_C(0x9e3779b97f4a7c15) ^ (uint64_t)(index + 1);
        if (stress_memory_bytes > 0)
        {
            worker->memory = stress_memory + offset;
            worker->memory_size = index + 1 == stress_thread_count
                                    ? stress_memory_bytes - offset
                                    : chunk_size;
        }
        worker->handle = CreateThread(NULL, 0, stress_worker_main, worker, 0, NULL);
        if (!worker->handle)
            return 0;
        ++stress_threads_started;
    }
    return 1;
}

static int
stop_stress_workers(void)
{
    int success = 1;

    InterlockedExchange(&stress_stop_requested, 1);
    if (stress_start_event)
        SetEvent(stress_start_event);
    for (DWORD index = 0; index < stress_threads_started; ++index)
    {
        DWORD wait = WaitForSingleObject(stress_workers[index].handle, 10000);
        if (wait != WAIT_OBJECT_0)
            success = 0;
        CloseHandle(stress_workers[index].handle);
        stress_workers[index].handle = NULL;
    }
    stress_threads_started = 0;
    if (success && stress_memory)
    {
        VirtualFree(stress_memory, 0, MEM_RELEASE);
        stress_memory = NULL;
    }
    if (stress_start_event)
    {
        CloseHandle(stress_start_event);
        stress_start_event = NULL;
    }
    return success;
}

static void
run_callback_work(void)
{
    LARGE_INTEGER started;
    LARGE_INTEGER now;
    uint64_t value = callback_work_sink ^ (uint64_t)InterlockedCompareExchange(&cycles, 0, 0);

    if (callback_work_ticks == 0)
        return;
    QueryPerformanceCounter(&started);
    do
    {
        for (LONG index = 0; index < 64; ++index)
        {
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
        }
        QueryPerformanceCounter(&now);
    } while (now.QuadPart - started.QuadPart < callback_work_ticks);
    callback_work_sink = value;
}

static void
observe_callback_scheduler(void)
{
    struct sched_param parameters = { 0 };
    int policy = sched_getscheduler(0);

    if (benchmark_rt_priority > 0 && !callback_sched_set_attempted)
    {
        callback_sched_set_attempted = 1;
        if (policy < 0 || sched_getparam(0, &parameters) != 0)
            callback_sched_set_error = errno;
        else if (benchmark_rt_priority > (DWORD)parameters.sched_priority)
            callback_sched_set_error = ERANGE;
        else
        {
            parameters.sched_priority = (int)benchmark_rt_priority;
            callback_sched_set_error = sched_setscheduler(
                                           0, SCHED_FIFO | SCHED_RESET_ON_FORK, &parameters)
                                           == 0
                                         ? 0
                                         : errno;
        }
    }
    if (callback_sched_policy != INT32_MIN)
        return;
    policy = sched_getscheduler(0);
    if (policy >= 0 && sched_getparam(0, &parameters) == 0)
        callback_sched_priority = parameters.sched_priority;
    else
        callback_sched_priority = -1;
    callback_sched_reset_on_fork = policy >= 0 && (policy & SCHED_RESET_ON_FORK) != 0;
    callback_sched_policy = policy < 0 ? policy : policy & ~SCHED_RESET_ON_FORK;
}

static void
run_probe_interval(DWORD duration_ms)
{
    DWORD elapsed = 0;

    if (!benchmark_enabled)
    {
        Sleep(duration_ms);
        return;
    }
    while (elapsed < duration_ms)
    {
        LARGE_INTEGER before;
        LARGE_INTEGER after;
        DWORD slice = duration_ms - elapsed;
        LONG64 expected_ticks;
        LONG64 gap_ticks;
        LONG64 late_ticks;

        if (slice > benchmark_heartbeat_ms)
            slice = benchmark_heartbeat_ms;
        QueryPerformanceCounter(&before);
        Sleep(slice);
        QueryPerformanceCounter(&after);
        expected_ticks = benchmark_frequency.QuadPart * slice / 1000;
        gap_ticks = after.QuadPart - before.QuadPart;
        late_ticks = gap_ticks > expected_ticks ? gap_ticks - expected_ticks : 0;
        update_max_ticks(&host_heartbeat_max_gap_ticks, gap_ticks);
        update_max_ticks(&host_heartbeat_max_late_ticks, late_ticks);
        InterlockedIncrement64(&host_heartbeats);
        elapsed += slice;
    }
}

static void
reset_benchmark(void)
{
    InterlockedExchange(&stress_stop_requested, 0);
    stress_passes = 0;
    stress_work_units = 0;
    stress_checksum = 0;
    callback_timed_cycles = 0;
    callback_total_ticks = 0;
    callback_max_ticks = 0;
    InterlockedExchange(&callback_period_overruns, 0);
    host_heartbeats = 0;
    host_heartbeat_max_gap_ticks = 0;
    host_heartbeat_max_late_ticks = 0;
    callback_sched_policy = INT32_MIN;
    callback_sched_priority = INT32_MIN;
    callback_sched_reset_on_fork = 0;
    callback_sched_set_attempted = 0;
    callback_sched_set_error = 0;
}

static int
start_benchmark(void)
{
    reset_benchmark();
    if (!prepare_stress_workers())
    {
        stop_stress_workers();
        return 0;
    }
    QueryPerformanceCounter(&benchmark_started);
    benchmark_running = 1;
    if (stress_start_event)
        SetEvent(stress_start_event);
    return 1;
}

static int
finish_benchmark(void)
{
    LARGE_INTEGER finished;
    LONG64 elapsed_ticks;
    LONG64 timed_cycles;
    LONG64 work_units;
    double elapsed_seconds;
    double callback_mean_us;
    int success;

    if (!benchmark_running)
        return 1;
    QueryPerformanceCounter(&finished);
    elapsed_ticks = finished.QuadPart - benchmark_started.QuadPart;
    benchmark_running = 0;
    success = stop_stress_workers();
    timed_cycles = InterlockedCompareExchange64(&callback_timed_cycles, 0, 0);
    work_units = InterlockedCompareExchange64(&stress_work_units, 0, 0);
    elapsed_seconds = (double)elapsed_ticks / (double)benchmark_frequency.QuadPart;
    callback_mean_us = timed_cycles == 0
                         ? 0.0
                         : (double)InterlockedCompareExchange64(&callback_total_ticks, 0, 0)
                               * 1000000.0
                               / ((double)benchmark_frequency.QuadPart * (double)timed_cycles);
    fprintf(stderr,
            "[asio-probe] benchmark callbacks: count=%llu mean_us=%.3f max_us=%.3f period_overruns=%ld sched_policy=%ld sched_priority=%ld sched_reset_on_fork=%ld sched_set_error=%ld\n",
            (unsigned long long)timed_cycles, callback_mean_us,
            (double)InterlockedCompareExchange64(&callback_max_ticks, 0, 0) * 1000000.0
                / (double)benchmark_frequency.QuadPart,
            (long)InterlockedCompareExchange(&callback_period_overruns, 0, 0),
            (long)callback_sched_policy, (long)callback_sched_priority,
            (long)callback_sched_reset_on_fork, (long)callback_sched_set_error);
    fprintf(stderr,
            "[asio-probe] benchmark host: heartbeats=%llu max_gap_us=%.3f max_late_us=%.3f elapsed_s=%.3f\n",
            (unsigned long long)InterlockedCompareExchange64(&host_heartbeats, 0, 0),
            (double)InterlockedCompareExchange64(&host_heartbeat_max_gap_ticks, 0, 0)
                * 1000000.0 / (double)benchmark_frequency.QuadPart,
            (double)InterlockedCompareExchange64(&host_heartbeat_max_late_ticks, 0, 0)
                * 1000000.0 / (double)benchmark_frequency.QuadPart,
            elapsed_seconds);
    fprintf(stderr,
            "[asio-probe] benchmark workers: threads=%lu memory_mib=%llu passes=%llu work_units=%llu work_units_per_s=%.0f checksum=%llu\n",
            (unsigned long)stress_thread_count,
            (unsigned long long)(stress_memory_bytes / (1024 * 1024)),
            (unsigned long long)InterlockedCompareExchange64(&stress_passes, 0, 0),
            (unsigned long long)work_units,
            elapsed_seconds > 0.0 ? (double)work_units / elapsed_seconds : 0.0,
            (unsigned long long)InterlockedCompareExchange64(&stress_checksum, 0, 0));
    if (benchmark_rt_priority > 0
        && (callback_sched_set_error != 0
            || callback_sched_priority != (LONG)benchmark_rt_priority))
        success = 0;
    return success;
}

static void
reset_loopback(void)
{
    loopback_frame = 0;
    callback_block_start = 0;
    next_pulse_frame = LOOPBACK_FIRST_PULSE_FRAME;
    pending_pulse_frame = 0;
    loopback_total = 0;
    pending_pulse = 0;
    loopback_measurements = 0;
    loopback_lost = 0;
    loopback_emitted = 0;
    loopback_emit = 1;
    callback_block_start_valid = 0;
    loopback_min = INT32_MAX;
    loopback_max = 0;
    last_callback_index = -1;
    callback_index_errors = 0;
    sample_position_seen = 0;
    sample_position_errors = 0;
    last_sample_position = 0;
}

static void
process_loopback(LONG index)
{
    float *input = active_buffers[4].buffers[index];
    float *output = active_buffers[10].buffers[index];
    uint64_t block_start = callback_block_start_valid
                             ? callback_block_start
                             : loopback_frame;
    uint64_t block_end = block_start + 64;

    if (sine_voices > 0)
        select_sine_stage(block_start);
    for (LONG frame = 0; frame < 64; ++frame)
    {
        float sample = input[frame];
        if (!pending_pulse || (sample < 0.125f && sample > -0.125f))
            continue;
        uint64_t delay = block_start + (uint64_t)frame - pending_pulse_frame;
        pending_pulse = 0;
        InterlockedIncrement(&loopback_measurements);
        loopback_total += delay;
        if ((LONG)delay < loopback_min)
            loopback_min = (LONG)delay;
        if ((LONG)delay > loopback_max)
            loopback_max = (LONG)delay;
        if (sine_voices > 0 && pending_pulse_stage >= 0)
        {
            ++sine_stats[pending_pulse_stage].loopback_count;
            if ((LONG)delay < sine_stats[pending_pulse_stage].loopback_min)
                sine_stats[pending_pulse_stage].loopback_min = (LONG)delay;
            if ((LONG)delay > sine_stats[pending_pulse_stage].loopback_max)
                sine_stats[pending_pulse_stage].loopback_max = (LONG)delay;
        }
        break;
    }
    if (pending_pulse
        && pending_pulse_frame + LOOPBACK_INTERVAL_FRAMES < block_end)
    {
        pending_pulse = 0;
        InterlockedIncrement(&loopback_lost);
    }

    memset(output, 0, 64 * sizeof(*output));
    while (next_pulse_frame < block_start)
        next_pulse_frame += LOOPBACK_INTERVAL_FRAMES;
    if (InterlockedCompareExchange(&loopback_emit, 0, 0) && next_pulse_frame < block_end)
    {
        if (pending_pulse)
            InterlockedIncrement(&loopback_lost);
        output[next_pulse_frame - block_start] = 0.25f;
        pending_pulse_frame = next_pulse_frame;
        pending_pulse_stage = sine_stage;
        pending_pulse = 1;
        InterlockedIncrement(&loopback_emitted);
        next_pulse_frame += LOOPBACK_INTERVAL_FRAMES;
    }
    loopback_frame = block_end;
    callback_block_start_valid = 0;
}

static void
print_loopback(const char *leg)
{
    double mean = loopback_measurements == 0
                    ? 0.0
                    : (double)loopback_total / (double)loopback_measurements;
    fprintf(stderr,
            "[asio-probe] %s loopback: emitted=%ld count=%ld lost=%ld pending=%ld min=%ld max=%ld mean=%.3f\n",
            leg, (long)loopback_emitted, (long)loopback_measurements, (long)loopback_lost,
            (long)pending_pulse,
            (long)(loopback_measurements == 0 ? 0 : loopback_min),
            (long)loopback_max, mean);
}

static void CALLBACK
buffer_switch(LONG index, LONG direct_process)
{
    NT_TIB *tib = (NT_TIB *)NtCurrentTeb();
    LONG thread = (LONG)GetCurrentThreadId();
    LONG previous;
    LONG previous_index;
    char marker;
    LARGE_INTEGER callback_started;
    LARGE_INTEGER callback_finished;

    (void)direct_process;
    if (benchmark_enabled)
    {
        QueryPerformanceCounter(&callback_started);
        observe_callback_scheduler();
    }
    if ((uintptr_t)&marker < (uintptr_t)tib->StackLimit
        || (uintptr_t)&marker >= (uintptr_t)tib->StackBase)
        InterlockedExchange(&invalid_callback_stack, 1);
    previous = InterlockedCompareExchange(&callback_thread, thread, 0);
    if ((previous != 0 && previous != thread) || thread == start_thread)
        InterlockedExchange(&callback_thread_mismatch, 1);
    InterlockedCompareExchange(&first_index, index, -1);
    if (InterlockedCompareExchange(&block_callback_once, 0, 1) == 1)
    {
        SetEvent(callback_entered_event);
        if (WaitForSingleObject(callback_release_event, 10000) != WAIT_OBJECT_0)
            InterlockedExchange(&callback_wait_errors, 1);
        SetEvent(callback_finished_event);
    }
    if (InterlockedCompareExchange(&callback_stop_once, 0, 1) == 1)
    {
        LONG result = active_asio ? active_asio->lpVtbl->Stop(active_asio) : INT32_MIN;
        InterlockedExchange(&callback_stop_result, result);
        SetEvent(callback_stop_event);
    }
    previous_index = InterlockedExchange(&last_callback_index, index);
    if (loopback_enabled && previous_index >= 0 && index != (previous_index ^ 1))
        InterlockedIncrement(&callback_index_errors);
    if (loopback_enabled)
        process_loopback(index);
    if (sine_voices > 0)
        process_sine(index);
    if (benchmark_enabled)
    {
        LONG64 duration;

        run_callback_work();
        QueryPerformanceCounter(&callback_finished);
        duration = callback_finished.QuadPart - callback_started.QuadPart;
        InterlockedIncrement64(&callback_timed_cycles);
        InterlockedExchangeAdd64(&callback_total_ticks, duration);
        update_max_ticks(&callback_max_ticks, duration);
        if (duration > callback_period_ticks)
            InterlockedIncrement(&callback_period_overruns);
        if (sine_voices > 0)
        {
            ++sine_stats[sine_stage].callbacks;
            sine_stats[sine_stage].total_ticks += duration;
            if (duration > sine_stats[sine_stage].max_ticks)
                sine_stats[sine_stage].max_ticks = duration;
            if (duration > callback_period_ticks)
                ++sine_stats[sine_stage].period_overruns;
        }
    }
    InterlockedIncrement(&cycles);
}

static void CALLBACK
sample_rate_changed(double rate)
{
    (void)rate;
}

static LONG CALLBACK
asio_message(LONG selector, LONG value, void *message, double *option)
{
    (void)value;
    (void)message;
    (void)option;
    return selector == 7;
}

static void *CALLBACK
buffer_switch_time_info(void *time_info, LONG index, LONG direct_process)
{
    AsioTimeInfo *info = time_info;
    if (loopback_enabled && info)
    {
        uint64_t position = ((uint64_t)info->sample_position.hi << 32)
                            | info->sample_position.lo;
        if (sample_position_seen && position != last_sample_position + 64)
            InterlockedIncrement(&sample_position_errors);
        last_sample_position = position;
        sample_position_seen = 1;
        callback_block_start = position;
        callback_block_start_valid = 1;
    }
    buffer_switch(index, direct_process);
    return NULL;
}

static int
fail(const char *operation, LONG result)
{
    fprintf(stderr, "[asio-probe] %s failed: %ld\n", operation, (long)result);
    return 1;
}

#define SINE_CHECK(condition) \
    do \
    { \
        if (!(condition)) \
        { \
            fprintf(stderr, "[asio-probe] sine self-test FAIL line=%d: %s\n", \
                    __LINE__, #condition); \
            return 1; \
        } \
    } while (0)

static int
run_sine_self_test(void)
{
    AsioBufferInfo buffers[18] = { 0 };
    static float audio[18][2][64];
    float delayed[4][64] = { 0 };
    const DWORD voice_counts[] = { 1, MAX_SINE_VOICES };

    active_buffers = buffers;
    for (LONG channel = 0; channel < 18; ++channel)
        for (LONG index = 0; index < 2; ++index)
            buffers[channel].buffers[index] = audio[channel][index];

    sine_voices = MAX_SINE_VOICES + 1;
    SINE_CHECK(!prepare_sine(1000));
    sine_voices = 1;
    SINE_CHECK(!prepare_sine(999));
    SINE_CHECK(prepare_sine(UINT32_MAX));
    SINE_CHECK(sine_total_frames == (uint64_t)UINT32_MAX * 48);
    sine_voices = 0;
    SINE_CHECK(!prepare_sine(1000));
    reset_loopback();
    process_loopback(0);
    process_sine(0);
    process_loopback(1);
    process_sine(1);
    for (LONG frame = 0; frame < 64; ++frame)
    {
        SINE_CHECK(audio[10][0][frame] == 0.0f);
        SINE_CHECK(audio[10][1][frame] == (frame == 1 ? 0.25f : 0.0f));
    }
    SINE_CHECK(pending_pulse_frame == 65 && next_pulse_frame == 65 + 4097);
    for (LONG sign = -1; sign <= 1; sign += 2)
    {
        memset(audio, 0, sizeof(audio));
        reset_loopback();
        process_loopback(0);
        process_loopback(1);
        for (LONG frame = 0; frame < 64; ++frame)
            audio[4][0][frame] = (float)(sign * SINE_PEAK);
        process_loopback(0);
        SINE_CHECK(pending_pulse && loopback_measurements == 0);
        audio[4][1][0] = (float)(0.25 + sign * SINE_PEAK);
        process_loopback(1);
        SINE_CHECK(!pending_pulse && loopback_measurements == 1 && loopback_min == 127);
    }

    for (size_t test = 0; test < sizeof(voice_counts) / sizeof(voice_counts[0]); ++test)
    {
        uint64_t origin = UINT64_C(1) << 32;
        uint64_t first_ns;
        double phase;
        double energy = 0.0;

        sine_voices = voice_counts[test];
        SINE_CHECK(prepare_sine(1000));
        reset_sine();
        select_sine_stage(origin);
        first_ns = sine_stats[0].first_mono_ns;
        SINE_CHECK(sine_stage == 0 && first_ns != 0 && stress_active == 0);
        memset(audio, 0, sizeof(audio));
        process_sine(0);
        phase = sine_oscillators[0].phase;
        SINE_CHECK(phase > 0.0);
        for (DWORD voice = 0; voice < sine_voices; ++voice)
            SINE_CHECK(fabs(sine_oscillators[voice].phase
                            - (voice == 0 ? fmod(sine_oscillators[voice].step * 64, SINE_TWO_PI)
                                          : 0.0)) < 1e-10);
        select_sine_stage(origin + 11999);
        SINE_CHECK(sine_stage == 0 && sine_stats[0].first_mono_ns == first_ns);
        select_sine_stage(origin + 12000);
        SINE_CHECK(sine_stage == 1 && stress_active == 1);
        SINE_CHECK(sine_stats[1].first_sample_position == origin + 12000);
        SINE_CHECK(sine_stats[1].first_mono_ns >= first_ns);
        SINE_CHECK(sine_oscillators[0].phase == phase);
        for (LONG block = 0; block < 16; ++block)
        {
            double expected_sample = 0.0;

            /* Reference sample 17 uses analytic phases, not renderer state or output. */
            for (DWORD voice = 0; voice < sine_voices; ++voice)
            {
                double frequency = 220.0 + (sine_voices > 1
                                             ? 1780.0 * voice / (sine_voices - 1) : 0.0);
                double step = SINE_TWO_PI * frequency / 48000.0;
                LONG samples = (voice == 0 ? 64 : 0) + block * 64 + 17;

                expected_sample += sin(fmod(step * samples, SINE_TWO_PI));
            }
            expected_sample *= SINE_PEAK / sine_voices;
            memset(audio[10][0], 0, sizeof(audio[10][0]));
            for (LONG channel = 11; channel < 18; ++channel)
                audio[channel][0][0] = 1.0f;
            process_sine(0);
            SINE_CHECK(fabs(audio[10][0][17] - expected_sample) < 1e-8);
            for (LONG frame = 0; frame < 64; ++frame)
            {
                float sample = audio[10][0][frame];

                SINE_CHECK(isfinite(sample) && fabs(sample) <= SINE_PEAK);
                SINE_CHECK(fabs(sample) < 0.125);
                SINE_CHECK(0.25f + sample >= 0.21875f && 0.25f + sample <= 0.28125f);
                SINE_CHECK(0.25f - sample >= 0.21875f && 0.25f - sample <= 0.28125f);
                energy += fabs(sample);
                for (LONG channel = 11; channel < 18; ++channel)
                    SINE_CHECK(audio[channel][0][frame] == 0.0f);
            }
        }
        SINE_CHECK(energy > 0.0);
        SINE_CHECK(sine_stats[1].voice_samples == (uint64_t)sine_voices * 16 * 64);
        for (DWORD voice = 0; voice < sine_voices; ++voice)
        {
            double frequency = 220.0 + (sine_voices > 1
                                         ? 1780.0 * voice / (sine_voices - 1) : 0.0);
            double step = SINE_TWO_PI * frequency / 48000.0;
            LONG samples = (voice == 0 ? 17 : 16) * 64;

            SINE_CHECK(isfinite(sine_oscillators[voice].phase));
            SINE_CHECK(sine_oscillators[voice].phase >= 0.0
                       && sine_oscillators[voice].phase < SINE_TWO_PI);
            SINE_CHECK(sine_oscillators[voice].step >= SINE_TWO_PI * 220.0 / 48000.0
                       && sine_oscillators[voice].step <= SINE_TWO_PI * 2000.0 / 48000.0);
            SINE_CHECK(fabs(sine_oscillators[voice].step - step) < 1e-15);
            SINE_CHECK(fabs(sine_oscillators[voice].phase
                            - fmod(step * samples, SINE_TWO_PI)) < 1e-10);
        }
        phase = sine_oscillators[0].phase;
        select_sine_stage(origin + 35999);
        SINE_CHECK(sine_stage == 1 && stress_active == 1);
        select_sine_stage(origin + 36000);
        SINE_CHECK(sine_stage == 2 && stress_active == 0);
        SINE_CHECK(sine_oscillators[0].phase == phase);
        SINE_CHECK(sine_stats[2].first_sample_position == origin + 36000);
        SINE_CHECK(sine_stats[2].first_mono_ns >= sine_stats[1].first_mono_ns);
        first_ns = sine_stats[2].first_mono_ns;
        select_sine_stage(origin + 48000);
        SINE_CHECK(sine_stage == 2 && sine_stats[2].first_mono_ns == first_ns);
        memset(audio[10][0], 0, sizeof(audio[10][0]));
        process_sine(0);
        for (DWORD voice = 0; voice < sine_voices; ++voice)
            SINE_CHECK(fabs(sine_oscillators[voice].phase
                            - fmod(sine_oscillators[voice].step * (voice == 0 ? 18 : 16) * 64,
                                   SINE_TWO_PI)) < 1e-10);
    }

    sine_voices = 8;
    SINE_CHECK(prepare_sine(1030));
    SINE_CHECK(QueryPerformanceFrequency(&benchmark_frequency));
    callback_period_ticks = benchmark_frequency.QuadPart * 64 / 48000;
    benchmark_enabled = 1;
    loopback_enabled = 1;
    stress_passes = 17;
    stress_work_units = 23;
    stress_checksum = 29;
    stress_stop_requested = 1;
    host_heartbeats = 31;
    for (LONG leg = 0; leg < 2; ++leg)
    {
        LONG cross_stage_observations = 0;
        LONG64 timed_before = callback_timed_cycles;

        memset(audio, 0, sizeof(audio));
        memset(delayed, 0, sizeof(delayed));
        reset_loopback();
        reset_sine();
        cycles = 0;
        first_index = -1;
        SINE_CHECK(sine_stage == -1 && pending_pulse_stage == -1 && stress_active == 0);
        SINE_CHECK(next_pulse_frame == 65 && !pending_pulse);
        SINE_CHECK(stress_passes == 17 && stress_work_units == 23 && stress_checksum == 29);
        SINE_CHECK(stress_stop_requested == 1 && host_heartbeats == 31);
        for (DWORD voice = 0; voice < sine_voices; ++voice)
            SINE_CHECK(sine_oscillators[voice].phase == 0.0 && sine_oscillators[voice].step > 0.0);
        for (LONG stage = 0; stage < SINE_STAGE_COUNT; ++stage)
        {
            SINE_CHECK(sine_stats[stage].callbacks == 0 && sine_stats[stage].voice_samples == 0);
            SINE_CHECK(sine_stats[stage].loopback_count == 0
                       && sine_stats[stage].loopback_min == INT32_MAX);
            SINE_CHECK(sine_stats[stage].total_ticks == 0 && sine_stats[stage].max_ticks == 0
                       && sine_stats[stage].period_overruns == 0);
            SINE_CHECK(sine_stats[stage].first_mono_ns == 0
                       && sine_stats[stage].first_sample_position == 0);
        }
        /* A four-block cable crosses both stage boundaries with these unchanged pulses. */
        for (uint64_t frame = 0; frame < sine_total_frames + 4800; frame += 64)
        {
            LONG index = (LONG)(frame / 64 % 2);
            LONG slot = (LONG)(frame / 64 % 4);
            LONG emitted_stage = pending_pulse_stage;
            LONG measured_before = loopback_measurements;
            AsioTimeInfo info = { 0 };

            memcpy(audio[4][index], delayed[slot], sizeof(delayed[slot]));
            if (frame >= sine_total_frames)
                loopback_emit = 0;
            info.sample_position.lo = (ULONG)frame;
            buffer_switch_time_info(&info, index, 1);
            memcpy(delayed[slot], audio[10][index], sizeof(delayed[slot]));
            if (loopback_measurements != measured_before && emitted_stage != sine_stage)
                ++cross_stage_observations;
            SINE_CHECK(stress_active == (frame >= 12360 && frame < 37080));
        }
        SINE_CHECK(cross_stage_observations == 2);
        SINE_CHECK(loopback_emitted == 13 && loopback_measurements == 13);
        SINE_CHECK(loopback_lost == 0 && pending_pulse == 0);
        SINE_CHECK(loopback_min == 256 && loopback_max == 256);
        SINE_CHECK(callback_index_errors == 0 && sample_position_errors == 0 && first_index == 0);
        SINE_CHECK(callback_timed_cycles == timed_before + cycles);
        SINE_CHECK(sine_stats[0].loopback_count == 4 && sine_stats[1].loopback_count == 6
                   && sine_stats[2].loopback_count == 3);
        SINE_CHECK(sine_stats[0].first_sample_position == 0
                   && sine_stats[1].first_sample_position == 12416
                   && sine_stats[2].first_sample_position == 37120);
        for (LONG stage = 0; stage < SINE_STAGE_COUNT; ++stage)
        {
            SINE_CHECK(sine_stats[stage].callbacks > 0 && sine_stats[stage].first_mono_ns > 0);
            SINE_CHECK(sine_stats[stage].voice_samples
                       == sine_stats[stage].callbacks * 64 * (stage == 1 ? sine_voices : 1));
            SINE_CHECK(sine_stats[stage].loopback_min == 256 && sine_stats[stage].loopback_max == 256);
        }
    }

    AsioTimeInfo shifted = { 0 };
    shifted.sample_position.lo = (ULONG)(last_sample_position + 65);
    buffer_switch_time_info(&shifted, last_callback_index, 1);
    SINE_CHECK(sample_position_errors == 1 && callback_index_errors == 1);
    memset(audio, 0, sizeof(audio));
    memset(delayed, 0, sizeof(delayed));
    reset_loopback();
    reset_sine();
    for (uint64_t frame = 0; frame < 5000; frame += 64)
    {
        LONG index = (LONG)(frame / 64 % 2);
        LONG slot = (LONG)(frame / 64 % 4);

        memcpy(audio[4][index], delayed[slot], sizeof(delayed[slot]));
        if (frame == 320)
        {
            audio[4][index][2] = audio[4][index][1];
            audio[4][index][1] = 0.0f;
        }
        /* Exercise the no-time-info fallback and the existing one-sample strict check. */
        buffer_switch(index, 1);
        memcpy(delayed[slot], audio[10][index], sizeof(delayed[slot]));
    }
    SINE_CHECK(loopback_measurements == 2 && loopback_lost == 0 && pending_pulse == 0);
    SINE_CHECK(loopback_min == 256 && loopback_max == 257);

    stress_thread_count = 1;
    for (LONG leg = 0; leg < 2; ++leg)
    {
        LONG64 before = InterlockedCompareExchange64(&stress_work_units, 0, 0);
        LONG64 loaded_units = leg == 0 ? 11 : 0;

        reset_sine();
        SINE_CHECK(InterlockedCompareExchange64(&stress_work_units, 0, 0) == before);
        select_sine_stage(0);
        SINE_CHECK(stress_active == 0 && sine_stats[0].worker_begin == before);
        InterlockedExchangeAdd64(&stress_work_units, 3);
        select_sine_stage(sine_total_frames / 4);
        SINE_CHECK(stress_active == 1 && sine_stats[0].worker_end == before + 3);
        SINE_CHECK(sine_stats[1].worker_begin == sine_stats[0].worker_end);
        InterlockedExchangeAdd64(&stress_work_units, loaded_units);
        select_sine_stage(sine_total_frames / 4 + 64);
        SINE_CHECK(stress_active == 1 && sine_stats[1].worker_begin == before + 3);
        select_sine_stage(sine_total_frames * 3 / 4);
        SINE_CHECK(stress_active == 0 && sine_stats[1].worker_end == before + 3 + loaded_units);
        SINE_CHECK(sine_stats[2].worker_begin == sine_stats[1].worker_end);
        InterlockedExchangeAdd64(&stress_work_units, 7);
        select_sine_stage(sine_total_frames * 3 / 4 + 64);
        SINE_CHECK(sine_stats[2].worker_begin == before + 3 + loaded_units);
        for (LONG stage = 0; stage < SINE_STAGE_COUNT; ++stage)
        {
            sine_stats[stage].callbacks = 1;
            sine_stats[stage].loopback_count = 1;
        }
        /* Quiescent fake workers allow spill, but previous-leg work cannot satisfy loaded. */
        SINE_CHECK(finish_sine_stages() == (leg == 0));
        SINE_CHECK(sine_stats[2].worker_end == before + 3 + loaded_units + 7);
        SINE_CHECK(sine_stats[2].worker_end
                   == InterlockedCompareExchange64(&stress_work_units, 0, 0));
        SINE_CHECK(stress_active == 0 && stress_stop_requested == 1);
    }
    stress_thread_count = 0;
    SINE_CHECK(finish_sine_stages());
    fprintf(stderr,
            "[asio-probe] sine self-test PASS: bounds=1..4096 stages/reset all-voice-phase/mix phase/peak legacy pulse/shift emission-stage two-legs worker-snapshots/per-leg\n");
    return 0;
}

#undef SINE_CHECK

int
main(void)
{
    HRESULT hr;
    SideAlsaAsio *asio = NULL;
    AsioBufferInfo buffers[18] = { 0 };
    AsioCallbacks callbacks = {
        buffer_switch,
        sample_rate_changed,
        asio_message,
        buffer_switch_time_info,
    };
    LONG result;
    LONG inputs = 0;
    LONG outputs = 0;
    LONG minimum = 0;
    LONG maximum = 0;
    LONG preferred = 0;
    LONG granularity = 0;
    LONG input_latency = 0;
    LONG output_latency = 0;
    LONG first_run_thread = 0;
    LONG first_loopback_count = 0;
    LONG first_loopback_lost = 0;
    LONG first_loopback_emitted = 0;
    LONG first_loopback_pending = 0;
    LONG first_loopback_min = 0;
    LONG first_loopback_max = 0;
    LONG first_index_errors = 0;
    LONG first_position_errors = 0;
    double rate = 0.0;
    int started = 0;
    int disposed = 0;
    int exit_code = 1;
    DWORD run_ms = SIDEALSA_ASIO_LOOPBACK_DEFAULT ? 10000 : 1000;
    char run_text[16];
    char loopback_text[2];
    char lifecycle_text[2];
    char expected_text[16];
    char expected_output_latency_text[16];
    char crash_text[2];
    char benchmark_text[2];
    LONG expected_loopback = -1;
    LONG expected_output_latency = 64;
    int crash_after_start = 0;
    int lifecycle_enabled = SIDEALSA_ASIO_LIFECYCLE_DEFAULT;
    DWORD stress_memory_mib = 0;
    DWORD callback_work_us = 0;
    DWORD sine_self_test = 0;
    int sine_stages_ok = 1;

    if (!read_environment_u32("SIDEALSA_ASIO_PROBE_SINE_SELF_TEST", 1, &sine_self_test))
        return 1;
    if (sine_self_test)
        return run_sine_self_test();
    if (!read_environment_u32("SIDEALSA_ASIO_PROBE_SINE_VOICES", MAX_SINE_VOICES,
                              &sine_voices))
        return 1;
    start_thread = (LONG)GetCurrentThreadId();

    if (sine_voices > 0)
    {
        if (!read_environment_u32("SIDEALSA_ASIO_PROBE_MS", UINT32_MAX, &run_ms))
            return 1;
    }
    else if (GetEnvironmentVariableA("SIDEALSA_ASIO_PROBE_MS", run_text, sizeof(run_text)) > 0)
        run_ms = (DWORD)strtoul(run_text, NULL, 10);
    loopback_enabled = SIDEALSA_ASIO_LOOPBACK_DEFAULT
                       || GetEnvironmentVariableA("SIDEALSA_ASIO_PROBE_LOOPBACK", loopback_text,
                                                  sizeof(loopback_text)) > 0;
    lifecycle_enabled = lifecycle_enabled
                        || GetEnvironmentVariableA("SIDEALSA_ASIO_PROBE_LIFECYCLE", lifecycle_text,
                                                   sizeof(lifecycle_text)) > 0;
    if (GetEnvironmentVariableA("SIDEALSA_ASIO_EXPECTED_LOOPBACK_FRAMES", expected_text,
                                sizeof(expected_text)) > 0)
        expected_loopback = (LONG)strtol(expected_text, NULL, 10);
    if (GetEnvironmentVariableA("SIDEALSA_ASIO_EXPECTED_OUTPUT_LATENCY",
                                expected_output_latency_text,
                                sizeof(expected_output_latency_text)) > 0)
        expected_output_latency = (LONG)strtol(expected_output_latency_text, NULL, 10);
    crash_after_start = GetEnvironmentVariableA("SIDEALSA_ASIO_CRASH_AFTER_START", crash_text,
                                                 sizeof(crash_text)) > 0;
    if (!read_environment_u32("SIDEALSA_ASIO_PROBE_STRESS_THREADS", MAX_STRESS_THREADS,
                              &stress_thread_count)
        || !read_environment_u32("SIDEALSA_ASIO_PROBE_STRESS_MEMORY_MIB", 4096,
                                 &stress_memory_mib)
        || !read_environment_u32("SIDEALSA_ASIO_PROBE_CALLBACK_WORK_US", 10000,
                                 &callback_work_us)
        || !read_environment_u32("SIDEALSA_ASIO_PROBE_HEARTBEAT_MS", 1000,
                                 &benchmark_heartbeat_ms)
        || !read_environment_u32("SIDEALSA_ASIO_PROBE_RT_PRIORITY", 99,
                                 &benchmark_rt_priority))
        return 1;
    if (sine_voices > 0)
    {
        loopback_enabled = 1;
        if (run_ms < 1000)
            return fail("sine mode requires at least 1000 ms per Start leg", -1);
    }
    benchmark_enabled = sine_voices > 0 || stress_thread_count > 0 || callback_work_us > 0
                        || benchmark_rt_priority > 0
                        || GetEnvironmentVariableA("SIDEALSA_ASIO_PROBE_BENCHMARK",
                                                   benchmark_text, sizeof(benchmark_text)) > 0;
    if (stress_memory_mib > 0 && stress_thread_count == 0)
        return fail("stress memory requires stress threads", -1);
    if (benchmark_enabled && benchmark_heartbeat_ms == 0)
        return fail("benchmark heartbeat must be nonzero", -1);
    if (benchmark_enabled && lifecycle_enabled)
        return fail("benchmark and lifecycle modes are incompatible", -1);
    if (sine_voices > 0 && !prepare_sine(run_ms))
        return fail("prepare sine/monotonic clock", -1);
    if (benchmark_enabled && !QueryPerformanceFrequency(&benchmark_frequency))
        return fail("QueryPerformanceFrequency", (LONG)GetLastError());
    stress_memory_bytes = (SIZE_T)stress_memory_mib * 1024 * 1024;
    callback_work_ticks = benchmark_enabled
                            ? benchmark_frequency.QuadPart * callback_work_us / 1000000
                            : 0;
    callback_period_ticks = benchmark_enabled
                              ? benchmark_frequency.QuadPart * 64 / 48000
                              : 0;
    if (benchmark_enabled)
    {
        fprintf(stderr,
                "[asio-probe] benchmark config: stress_threads=%lu memory_mib=%lu callback_work_us=%lu heartbeat_ms=%lu rt_priority=%lu\n",
                (unsigned long)stress_thread_count, (unsigned long)stress_memory_mib,
                (unsigned long)callback_work_us, (unsigned long)benchmark_heartbeat_ms,
                (unsigned long)benchmark_rt_priority);
    }
    if (sine_voices > 0)
        fprintf(stderr,
                "[asio-probe] sine config: voices=%lu run_ms=%lu total_frames=%llu peak=0.03125 warm_percent=25 loaded_percent=50 cool_percent=25 worker_gate=loaded\n",
                (unsigned long)sine_voices, (unsigned long)run_ms,
                (unsigned long long)sine_total_frames);

    hr = CoInitializeEx(NULL, COINIT_APARTMENTTHREADED);
    if (FAILED(hr))
        return fail("CoInitializeEx", hr);

    hr = CoCreateInstance(&CLSID_SideAlsaAsio, NULL, CLSCTX_INPROC_SERVER, &CLSID_SideAlsaAsio,
                          (void **)&asio);
    if (FAILED(hr) || !asio)
    {
        fprintf(stderr, "[asio-probe] CoCreateInstance failed: 0x%lx\n", (unsigned long)hr);
        goto done;
    }

    void *query = NULL;
    GUID unsupported = { 0x7c4492ae, 0x7920, 0x41d8, { 0xa2, 0xe6, 0x55, 0x66, 0x12, 0x31, 0x8e, 0x7c } };
    if (asio->lpVtbl->QueryInterface(asio, &unsupported, &query) != E_NOINTERFACE || query)
    {
        exit_code = fail("unsupported QueryInterface", -1);
        goto release;
    }
    if (asio->lpVtbl->QueryInterface(asio, &IID_IUnknown, &query) != S_OK || query != asio)
    {
        exit_code = fail("IUnknown QueryInterface", -1);
        goto release;
    }
    ((SideAlsaAsio *)query)->lpVtbl->Release(query);
    if (asio->lpVtbl->QueryInterface(asio, &CLSID_SideAlsaAsio, &query) != S_OK || query != asio)
    {
        exit_code = fail("ASIO QueryInterface", -1);
        goto release;
    }
    ((SideAlsaAsio *)query)->lpVtbl->Release(query);
    if (asio->lpVtbl->QueryInterface(asio, &IID_IUnknown, NULL) != E_POINTER)
    {
        exit_code = fail("null QueryInterface", -1);
        goto release;
    }

    AsioClockSource clock = { 0 };
    LONG clock_count = 1;
    if (asio->lpVtbl->GetClockSources(asio, &clock, &clock_count) != 0 || clock_count != 1
        || strcmp(clock.name, "Internal") != 0)
    {
        exit_code = fail("GetClockSources", -1);
        goto release;
    }

    result = asio->lpVtbl->Init(asio, NULL);
    if (result != 1)
    {
        char message[124] = { 0 };
        asio->lpVtbl->GetErrorMessage(asio, message);
        fprintf(stderr, "[asio-probe] Init unavailable: %s (%ld)\n", message, (long)result);
        exit_code = 77;
        goto release;
    }
    fprintf(stderr, "[asio-probe] Init OK\n");

    result = asio->lpVtbl->GetChannels(asio, &inputs, &outputs);
    if (result != 0 || inputs != 10 || outputs != 8)
    {
        exit_code = fail("GetChannels", result);
        goto release;
    }
    result = asio->lpVtbl->GetBufferSize(asio, &minimum, &maximum, &preferred, &granularity);
    if (result != 0 || minimum != 64 || maximum != 64 || preferred != 64 || granularity != 0)
    {
        exit_code = fail("GetBufferSize", result);
        goto release;
    }
    result = asio->lpVtbl->GetSampleRate(asio, &rate);
    if (result != 0 || rate != 48000.0 || asio->lpVtbl->CanSampleRate(asio, 48000.0) != 0)
    {
        exit_code = fail("sample rate", result);
        goto release;
    }
    result = asio->lpVtbl->GetLatencies(asio, &input_latency, &output_latency);
    if (result != 0 || input_latency != 64 || output_latency != expected_output_latency)
    {
        exit_code = fail("GetLatencies", result);
        goto release;
    }

    for (LONG channel = 0; channel < 10; ++channel)
    {
        AsioChannelInfo info = { .channel = channel, .is_input = 1, .is_active = 123 };
        result = asio->lpVtbl->GetChannelInfo(asio, &info);
        if (result != 0 || info.sample_type != 19 || info.is_active != 0)
        {
            exit_code = fail("input channel info", result);
            goto release;
        }
        buffers[channel].is_input_type = 1;
        buffers[channel].channel_number = channel;
    }
    for (LONG channel = 0; channel < 8; ++channel)
    {
        AsioChannelInfo info = { .channel = channel, .is_input = 0, .is_active = 123 };
        result = asio->lpVtbl->GetChannelInfo(asio, &info);
        if (result != 0 || info.sample_type != 19 || info.is_active != 0)
        {
            exit_code = fail("output channel info", result);
            goto release;
        }
        buffers[10 + channel].channel_number = channel;
    }

    AsioCallbacks invalid_callbacks = callbacks;
    invalid_callbacks.buffer_switch = NULL;
    result = asio->lpVtbl->CreateBuffers(asio, buffers, 18, 64, &invalid_callbacks);
    if (result != -998)
    {
        exit_code = fail("invalid callback table", result);
        goto release;
    }
    result = asio->lpVtbl->CreateBuffers(asio, buffers, 18, 64, &callbacks);
    if (result != 0)
    {
        exit_code = fail("CreateBuffers", result);
        goto release;
    }
    fprintf(stderr, "[asio-probe] CreateBuffers OK\n");
    active_buffers = buffers;
    active_asio = asio;
    for (size_t index = 0; index < sizeof(buffers) / sizeof(buffers[0]); ++index)
    {
        if (!buffers[index].buffers[0] || !buffers[index].buffers[1])
        {
            exit_code = fail("buffer pointers", -1);
            goto dispose;
        }
    }
    AsioChannelInfo active_info = { .channel = 0, .is_input = 1 };
    result = asio->lpVtbl->GetChannelInfo(asio, &active_info);
    if (result != 0 || active_info.is_active != 1)
    {
        exit_code = fail("active channel info", result);
        goto dispose;
    }
    if (lifecycle_enabled)
    {
        callback_entered_event = CreateEventA(NULL, TRUE, FALSE, NULL);
        callback_release_event = CreateEventA(NULL, TRUE, FALSE, NULL);
        callback_finished_event = CreateEventA(NULL, TRUE, FALSE, NULL);
        callback_stop_event = CreateEventA(NULL, TRUE, FALSE, NULL);
        if (!callback_entered_event || !callback_release_event || !callback_finished_event
            || !callback_stop_event)
        {
            exit_code = fail("lifecycle events", (LONG)GetLastError());
            goto dispose;
        }
    }

    InterlockedExchange(&cycles, 0);
    InterlockedExchange(&first_index, -1);
    InterlockedExchange(&callback_thread, 0);
    InterlockedExchange(&callback_thread_mismatch, 0);
    InterlockedExchange(&invalid_callback_stack, 0);
    if (loopback_enabled)
        reset_loopback();
    if (lifecycle_enabled)
    {
        ResetEvent(callback_entered_event);
        ResetEvent(callback_release_event);
        ResetEvent(callback_finished_event);
        InterlockedExchange(&callback_wait_errors, 0);
        InterlockedExchange(&block_callback_once, 1);
    }
    if (benchmark_enabled && !start_benchmark())
    {
        exit_code = fail("start benchmark workers", (LONG)GetLastError());
        goto dispose;
    }
    if (sine_voices > 0)
        reset_sine();
    result = asio->lpVtbl->Start(asio);
    if (result != 0)
    {
        exit_code = fail("Start", result);
        goto dispose;
    }
    started = 1;
    if (lifecycle_enabled)
    {
        DWORD stop_started;
        DWORD stop_elapsed;
        DWORD retry_started;

        if (WaitForSingleObject(callback_entered_event, 5000) != WAIT_OBJECT_0)
        {
            SetEvent(callback_release_event);
            exit_code = fail("blocked callback entry", (LONG)GetLastError());
            goto dispose;
        }
        stop_started = GetTickCount();
        result = asio->lpVtbl->Stop(asio);
        stop_elapsed = GetTickCount() - stop_started;
        SetEvent(callback_release_event);
        if (result == 0 || stop_elapsed < 750 || stop_elapsed > 1500)
        {
            fprintf(stderr, "[asio-probe] blocked Stop result=%ld elapsed=%lu ms\n",
                    (long)result, (unsigned long)stop_elapsed);
            exit_code = fail("bounded blocked-callback Stop", result);
            goto dispose;
        }
        fprintf(stderr, "[asio-probe] blocked Stop bounded at %lu ms\n",
                (unsigned long)stop_elapsed);
        retry_started = GetTickCount();
        do
        {
            result = asio->lpVtbl->Stop(asio);
            if (result != 0)
                Sleep(10);
        } while (result != 0 && GetTickCount() - retry_started < 2500);
        if (result != 0 || callback_wait_errors != 0)
        {
            exit_code = fail("blocked callback Stop finalization", result);
            goto dispose;
        }
        started = 0;
    }
    else
    {
        run_probe_interval(run_ms);
        if (crash_after_start)
        {
            LONG crash_cycles = InterlockedCompareExchange(&cycles, 0, 0);
            LONG crash_measurements = InterlockedCompareExchange(&loopback_measurements, 0, 0);
            LONG crash_emitted = InterlockedCompareExchange(&loopback_emitted, 0, 0);
            fprintf(stderr, "[asio-probe] crash loopback: emitted=%ld count=%ld\n",
                    (long)crash_emitted, (long)crash_measurements);
            if (crash_cycles == 0 || crash_measurements == 0)
            {
                exit_code = fail("crash precondition", -1);
                goto dispose;
            }
            fprintf(stderr, "[asio-probe] intentional process crash\n");
            fflush(stderr);
            if (!TerminateProcess(GetCurrentProcess(), 99))
            {
                exit_code = fail("TerminateProcess", (LONG)GetLastError());
                goto dispose;
            }
            Sleep(INFINITE);
        }
        if (loopback_enabled)
        {
            InterlockedExchange(&loopback_emit, 0);
            Sleep(100);
        }
        result = asio->lpVtbl->Stop(asio);
        started = 0;
    }
    if (result == 0 && sine_voices > 0)
        sine_stages_ok &= print_sine_stages("first");
    if (result != 0 || cycles == 0 || first_index != 0 || callback_thread == 0
        || callback_thread_mismatch != 0 || invalid_callback_stack != 0)
    {
        exit_code = fail("Stop/callbacks", result);
        goto dispose;
    }
    LONG stopped_cycles = cycles;
    first_run_thread = callback_thread;
    if (loopback_enabled)
    {
        print_loopback("first");
        first_loopback_count = loopback_measurements;
        first_loopback_lost = loopback_lost;
        first_loopback_emitted = loopback_emitted;
        first_loopback_pending = pending_pulse;
        first_loopback_min = loopback_min;
        first_loopback_max = loopback_max;
        first_index_errors = callback_index_errors;
        first_position_errors = sample_position_errors;
    }
    Sleep(20);
    if (cycles != stopped_cycles)
    {
        exit_code = fail("callback after Stop", -1);
        goto dispose;
    }

    InterlockedExchange(&cycles, 0);
    InterlockedExchange(&first_index, -1);
    InterlockedExchange(&callback_thread, 0);
    InterlockedExchange(&callback_thread_mismatch, 0);
    if (loopback_enabled)
        reset_loopback();
    if (lifecycle_enabled)
    {
        ResetEvent(callback_stop_event);
        InterlockedExchange(&callback_stop_result, INT32_MIN);
        InterlockedExchange(&callback_stop_once, 1);
    }
    if (sine_voices > 0)
        reset_sine();
    result = asio->lpVtbl->Start(asio);
    if (result != 0)
    {
        exit_code = fail("restart", result);
        goto dispose;
    }
    started = 1;
    if (lifecycle_enabled)
    {
        if (WaitForSingleObject(callback_stop_event, 5000) != WAIT_OBJECT_0)
        {
            exit_code = fail("callback-thread Stop completion", (LONG)GetLastError());
            goto dispose;
        }
        result = asio->lpVtbl->Stop(asio);
        started = 0;
        if (callback_stop_result != 0)
        {
            exit_code = fail("callback-thread Stop", callback_stop_result);
            goto dispose;
        }
        fprintf(stderr, "[asio-probe] callback-thread Stop OK\n");
    }
    else
    {
        run_probe_interval(run_ms);
        if (loopback_enabled)
        {
            InterlockedExchange(&loopback_emit, 0);
            Sleep(100);
        }
        result = asio->lpVtbl->Stop(asio);
        started = 0;
    }
    if (result == 0 && sine_voices > 0)
        sine_stages_ok &= print_sine_stages("second");
    if (benchmark_enabled && !finish_benchmark())
    {
        exit_code = fail("benchmark finalization", (LONG)GetLastError());
        goto dispose;
    }
    if (result != 0 || cycles == 0 || first_index != 0 || callback_thread == 0
        || callback_thread != first_run_thread
        || callback_thread_mismatch != 0 || invalid_callback_stack != 0)
    {
        exit_code = fail("restart Stop/callbacks", result);
        goto dispose;
    }
    exit_code = 0;
    if (!sine_stages_ok)
        exit_code = fail("sine stage coverage/timing", -1);
    fprintf(stderr, "[asio-probe] callbacks: %ld\n", (long)cycles);
    if (loopback_enabled)
    {
        print_loopback("second");
        if (first_loopback_count == 0 || loopback_measurements == 0
            || first_loopback_lost != 0 || loopback_lost != 0
            || first_loopback_pending != 0 || pending_pulse != 0
            || first_loopback_emitted != first_loopback_count + first_loopback_lost
            || loopback_emitted != loopback_measurements + loopback_lost
            || first_loopback_min != first_loopback_max
            || loopback_min != loopback_max
            || first_loopback_min != loopback_min
            || (expected_loopback >= 0
                && (first_loopback_min != expected_loopback
                    || loopback_min != expected_loopback))
            || first_index_errors != 0 || callback_index_errors != 0
            || first_position_errors != 0 || sample_position_errors != 0)
        {
            exit_code = fail("loopback stability", -1);
        }
    }
    if (lifecycle_enabled && exit_code == 0)
    {
        DWORD release_started;
        DWORD release_elapsed;
        ULONG release_result;

        InterlockedExchange(&cycles, 0);
        ResetEvent(callback_entered_event);
        ResetEvent(callback_release_event);
        ResetEvent(callback_finished_event);
        InterlockedExchange(&block_callback_once, 1);
        result = asio->lpVtbl->Start(asio);
        if (result != 0)
        {
            exit_code = fail("final Release start", result);
            goto dispose;
        }
        started = 1;
        if (WaitForSingleObject(callback_entered_event, 5000) != WAIT_OBJECT_0)
        {
            SetEvent(callback_release_event);
            exit_code = fail("final Release callback entry", (LONG)GetLastError());
            goto dispose;
        }
        release_started = GetTickCount();
        release_result = asio->lpVtbl->Release(asio);
        release_elapsed = GetTickCount() - release_started;
        asio = NULL;
        active_asio = NULL;
        started = 0;
        SetEvent(callback_release_event);
        if (WaitForSingleObject(callback_finished_event, 5000) != WAIT_OBJECT_0)
        {
            exit_code = fail("final Release callback exit", (LONG)GetLastError());
            disposed = 0;
        }
        else if (release_result != 0 || release_elapsed > 500)
        {
            exit_code = fail("nonblocking final Release", (LONG)release_result);
            disposed = 1;
        }
        else
        {
            fprintf(stderr, "[asio-probe] final Release returned in %lu ms\n",
                    (unsigned long)release_elapsed);
            disposed = 1;
        }
        goto done;
    }

dispose:
    if (callback_release_event)
        SetEvent(callback_release_event);
    if (started)
        asio->lpVtbl->Stop(asio);
    if (asio->lpVtbl->DisposeBuffers(asio) != 0)
        exit_code = fail("DisposeBuffers", -1);
    else
        disposed = 1;
release:
    active_asio = NULL;
    asio->lpVtbl->Release(asio);
done:
    if (benchmark_running && !finish_benchmark() && exit_code == 0)
        exit_code = fail("benchmark finalization", (LONG)GetLastError());
    if (disposed)
    {
        if (callback_entered_event)
            CloseHandle(callback_entered_event);
        if (callback_release_event)
            CloseHandle(callback_release_event);
        if (callback_finished_event)
            CloseHandle(callback_finished_event);
        if (callback_stop_event)
            CloseHandle(callback_stop_event);
    }
    CoUninitialize();
    if (exit_code == 0)
        fprintf(stderr, "[asio-probe] PASS\n");
    return exit_code;
}
