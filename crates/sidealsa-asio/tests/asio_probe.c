/* SPDX-License-Identifier: GPL-3.0-or-later */

#define WIN32_LEAN_AND_MEAN
#define COBJMACROS

#include <objbase.h>
#include <windows.h>

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct
{
    ULONG hi;
    ULONG lo;
} AsioInt64;

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

static void CALLBACK
buffer_switch(LONG index, LONG direct_process)
{
    NT_TIB *tib = (NT_TIB *)NtCurrentTeb();
    LONG thread = (LONG)GetCurrentThreadId();
    LONG previous;
    char marker;

    (void)direct_process;
    if ((uintptr_t)&marker < (uintptr_t)tib->StackLimit
        || (uintptr_t)&marker >= (uintptr_t)tib->StackBase)
        InterlockedExchange(&invalid_callback_stack, 1);
    previous = InterlockedCompareExchange(&callback_thread, thread, 0);
    if ((previous != 0 && previous != thread) || thread == start_thread)
        InterlockedExchange(&callback_thread_mismatch, 1);
    InterlockedCompareExchange(&first_index, index, -1);
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
    (void)time_info;
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
    double rate = 0.0;
    int started = 0;
    int exit_code = 1;
    DWORD run_ms = 1000;
    char run_text[16];

    start_thread = (LONG)GetCurrentThreadId();

    if (GetEnvironmentVariableA("SIDEALSA_ASIO_PROBE_MS", run_text, sizeof(run_text)) > 0)
        run_ms = (DWORD)strtoul(run_text, NULL, 10);

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

    for (LONG channel = 0; channel < 10; ++channel)
    {
        AsioChannelInfo info = { .channel = channel, .is_input = 1 };
        result = asio->lpVtbl->GetChannelInfo(asio, &info);
        if (result != 0 || info.sample_type != 19)
        {
            exit_code = fail("input channel info", result);
            goto release;
        }
        buffers[channel].is_input_type = 1;
        buffers[channel].channel_number = channel;
    }
    for (LONG channel = 0; channel < 8; ++channel)
    {
        AsioChannelInfo info = { .channel = channel, .is_input = 0 };
        result = asio->lpVtbl->GetChannelInfo(asio, &info);
        if (result != 0 || info.sample_type != 19)
        {
            exit_code = fail("output channel info", result);
            goto release;
        }
        buffers[10 + channel].channel_number = channel;
    }

    result = asio->lpVtbl->CreateBuffers(asio, buffers, 18, 64, &callbacks);
    if (result != 0)
    {
        exit_code = fail("CreateBuffers", result);
        goto release;
    }
    fprintf(stderr, "[asio-probe] CreateBuffers OK\n");
    for (size_t index = 0; index < sizeof(buffers) / sizeof(buffers[0]); ++index)
    {
        if (!buffers[index].buffers[0] || !buffers[index].buffers[1])
        {
            exit_code = fail("buffer pointers", -1);
            goto dispose;
        }
    }

    InterlockedExchange(&cycles, 0);
    InterlockedExchange(&first_index, -1);
    InterlockedExchange(&callback_thread, 0);
    InterlockedExchange(&callback_thread_mismatch, 0);
    InterlockedExchange(&invalid_callback_stack, 0);
    result = asio->lpVtbl->Start(asio);
    if (result != 0)
    {
        exit_code = fail("Start", result);
        goto dispose;
    }
    started = 1;
    Sleep(run_ms);
    result = asio->lpVtbl->Stop(asio);
    started = 0;
    if (result != 0 || cycles == 0 || first_index != 0 || callback_thread == 0
        || callback_thread_mismatch != 0 || invalid_callback_stack != 0)
    {
        exit_code = fail("Stop/callbacks", result);
        goto dispose;
    }
    LONG stopped_cycles = cycles;
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
    result = asio->lpVtbl->Start(asio);
    if (result != 0)
    {
        exit_code = fail("restart", result);
        goto dispose;
    }
    started = 1;
    Sleep(run_ms);
    result = asio->lpVtbl->Stop(asio);
    started = 0;
    if (result != 0 || cycles == 0 || first_index != 0 || callback_thread == 0
        || callback_thread_mismatch != 0 || invalid_callback_stack != 0)
    {
        exit_code = fail("restart Stop/callbacks", result);
        goto dispose;
    }
    exit_code = 0;
    fprintf(stderr, "[asio-probe] callbacks: %ld\n", (long)cycles);

dispose:
    if (started)
        asio->lpVtbl->Stop(asio);
    if (asio->lpVtbl->DisposeBuffers(asio) != 0)
        exit_code = fail("DisposeBuffers", -1);
release:
    asio->lpVtbl->Release(asio);
done:
    CoUninitialize();
    return exit_code;
}
