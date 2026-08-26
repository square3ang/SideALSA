/* SPDX-License-Identifier: GPL-3.0-or-later */

#define WIN32_LEAN_AND_MEAN
#define COBJMACROS

#include <objbase.h>
#include <windows.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef SIDEALSA_ASIO_LOOPBACK_DEFAULT
#define SIDEALSA_ASIO_LOOPBACK_DEFAULT 0
#endif

#ifndef SIDEALSA_ASIO_LIFECYCLE_DEFAULT
#define SIDEALSA_ASIO_LIFECYCLE_DEFAULT 0
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

    (void)direct_process;
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
    LONG expected_loopback = -1;
    LONG expected_output_latency = 64;
    int crash_after_start = 0;
    int lifecycle_enabled = SIDEALSA_ASIO_LIFECYCLE_DEFAULT;

    start_thread = (LONG)GetCurrentThreadId();

    if (GetEnvironmentVariableA("SIDEALSA_ASIO_PROBE_MS", run_text, sizeof(run_text)) > 0)
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
        Sleep(run_ms);
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
        Sleep(run_ms);
        if (loopback_enabled)
        {
            InterlockedExchange(&loopback_emit, 0);
            Sleep(100);
        }
        result = asio->lpVtbl->Stop(asio);
        started = 0;
    }
    if (result != 0 || cycles == 0 || first_index != 0 || callback_thread == 0
        || callback_thread != first_run_thread
        || callback_thread_mismatch != 0 || invalid_callback_stack != 0)
    {
        exit_code = fail("restart Stop/callbacks", result);
        goto dispose;
    }
    exit_code = 0;
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
