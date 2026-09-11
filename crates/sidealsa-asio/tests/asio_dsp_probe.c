/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Reuse ABI declarations, as asio_split_probe.c does. Never run its Q64 main. */
#define main sidealsa_unused_reference_main
#include "asio_probe.c"
#undef main

static HANDLE dsp_kick[8], dsp_done[8], dsp_threads[8];
static volatile LONG dsp_quit, dsp_errors, dsp_count;
static LONG dsp_nworkers, dsp_frames;
static LONGLONG dsp_ticks, dsp_total, dsp_max;
static LARGE_INTEGER dsp_frequency;
static AsioBufferInfo dsp_buffers[64];
static LONG dsp_outputs;
static volatile uint64_t dsp_checksums[9];

static void dsp_work(unsigned slot)
{
    LARGE_INTEGER start, now;
    uint64_t v = dsp_checksums[slot] + 1;
    QueryPerformanceCounter(&start);
    do {
        for (unsigned i = 0; i < 256; ++i) v = v * UINT64_C(6364136223846793005) + 1;
        QueryPerformanceCounter(&now);
    } while (now.QuadPart - start.QuadPart < dsp_ticks);
    dsp_checksums[slot] = v;
}
static DWORD WINAPI dsp_worker(void *arg)
{
    unsigned i = (unsigned)(uintptr_t)arg;
    for (;;) {
        if (WaitForSingleObject(dsp_kick[i], INFINITE) != WAIT_OBJECT_0) return 1;
        if (InterlockedCompareExchange(&dsp_quit, 0, 0)) return 0;
        dsp_work(i);
        SetEvent(dsp_done[i]);
    }
}
static void CALLBACK dsp_callback(LONG index, LONG direct)
{
    LARGE_INTEGER begin, end;
    (void)direct;
    if (index < 0 || index > 1) { InterlockedIncrement(&dsp_errors); return; }
    QueryPerformanceCounter(&begin);
    /* Prime valid output before applying load, so deadline accounting is armed. */
    if (dsp_count < 32) {
        /* Silent warmup. */
    } else if (dsp_nworkers) {
        for (LONG i = 0; i < dsp_nworkers; ++i) SetEvent(dsp_kick[i]);
        if (WaitForMultipleObjects(dsp_nworkers, dsp_done, TRUE, 1000) != WAIT_OBJECT_0)
            InterlockedIncrement(&dsp_errors);
    } else dsp_work(8);
    for (LONG c = 0; c < dsp_outputs; ++c)
        memset(dsp_buffers[c].buffers[index], 0, dsp_frames * sizeof(float));
    QueryPerformanceCounter(&end);
    LONGLONG elapsed = end.QuadPart - begin.QuadPart;
    dsp_total += elapsed;
    if (elapsed > dsp_max) dsp_max = elapsed;
    InterlockedIncrement(&dsp_count);
}
static LONG CALLBACK dsp_message(LONG selector, LONG value, void *msg, double *opt)
{ (void)selector; (void)value; (void)msg; (void)opt; return 0; }
static void CALLBACK dsp_rate(double rate) { (void)rate; }

int main(int argc, char **argv)
{
    SideAlsaAsio *driver = NULL;
    IClassFactory *factory = NULL;
    HMODULE module = NULL;
    HRESULT (WINAPI *get_factory)(REFCLSID, REFIID, void **);
    LONG inputs, minimum, maximum, granularity;
    unsigned work_us, run_ms;
    int status = 1, created = 0, started = 0;
    if (argc != 4) { fprintf(stderr, "usage: asio-dsp-probe work_us workers(0..8) run_ms\n"); return 2; }
    work_us = strtoul(argv[1], NULL, 10);
    dsp_nworkers = strtoul(argv[2], NULL, 10);
    run_ms = strtoul(argv[3], NULL, 10);
    if (work_us > 10000 || dsp_nworkers < 0 || dsp_nworkers > 8 || run_ms < 1000 || run_ms > 30000) return 2;
    if (FAILED(CoInitializeEx(NULL, COINIT_APARTMENTTHREADED))) return 1;
#define DSP_CHECK(x) do { if (!(x)) { fprintf(stderr, "DSP probe failed line %d: %s\n", __LINE__, #x); goto cleanup; } } while (0)
    DSP_CHECK(QueryPerformanceFrequency(&dsp_frequency));
    dsp_ticks = dsp_frequency.QuadPart * work_us / 1000000;
    module = LoadLibraryA("sidealsa-asio64.dll"); DSP_CHECK(module);
    get_factory = (void *)GetProcAddress(module, "DllGetClassObject"); DSP_CHECK(get_factory);
    DSP_CHECK(SUCCEEDED(get_factory(&CLSID_SideAlsaAsio, &IID_IClassFactory, (void **)&factory)));
    DSP_CHECK(SUCCEEDED(factory->lpVtbl->CreateInstance(factory, NULL, &CLSID_SideAlsaAsio, (void **)&driver)));
    DSP_CHECK(driver->lpVtbl->Init(driver, NULL) == 1);
    DSP_CHECK(driver->lpVtbl->GetChannels(driver, &inputs, &dsp_outputs) == 0 && dsp_outputs > 0 && dsp_outputs < 64);
    DSP_CHECK(driver->lpVtbl->GetBufferSize(driver, &minimum, &maximum, &dsp_frames, &granularity) == 0);
    for (LONG i = 0; i < dsp_nworkers; ++i) {
        dsp_kick[i] = CreateEventA(NULL, FALSE, FALSE, NULL);
        dsp_done[i] = CreateEventA(NULL, FALSE, FALSE, NULL);
        DSP_CHECK(dsp_kick[i] && dsp_done[i]);
        dsp_threads[i] = CreateThread(NULL, 0, dsp_worker, (void *)(uintptr_t)i, 0, NULL);
        DSP_CHECK(dsp_threads[i]);
    }
    for (LONG c = 0; c < dsp_outputs; ++c) dsp_buffers[c].channel_number = c;
    if (inputs > 0) dsp_buffers[dsp_outputs].is_input_type = 1;
    AsioCallbacks cb = { dsp_callback, dsp_rate, dsp_message, NULL };
    DSP_CHECK(driver->lpVtbl->CreateBuffers(driver, dsp_buffers, dsp_outputs + (inputs > 0), dsp_frames, &cb) == 0);
    created = 1;
    DSP_CHECK(driver->lpVtbl->Start(driver) == 0); started = 1;
    Sleep(run_ms);
    DSP_CHECK(driver->lpVtbl->Stop(driver) == 0); started = 0;
    printf("frames=%ld workers=%ld work_us=%u callbacks=%ld errors=%ld mean_us=%.3f max_us=%.3f\n",
        (long)dsp_frames, (long)dsp_nworkers, work_us, (long)dsp_count, (long)dsp_errors,
        dsp_count ? (double)dsp_total * 1000000.0 / dsp_frequency.QuadPart / dsp_count : 0,
        (double)dsp_max * 1000000.0 / dsp_frequency.QuadPart);
    status = dsp_errors || !dsp_count;
cleanup:
    if (started && driver) driver->lpVtbl->Stop(driver);
    if (created && driver) driver->lpVtbl->DisposeBuffers(driver);
    if (driver) driver->lpVtbl->Release(driver);
    InterlockedExchange(&dsp_quit, 1);
    for (LONG i = 0; i < dsp_nworkers; ++i) {
        if (dsp_kick[i]) SetEvent(dsp_kick[i]);
        if (dsp_threads[i]) { WaitForSingleObject(dsp_threads[i], 3000); CloseHandle(dsp_threads[i]); }
        if (dsp_kick[i]) CloseHandle(dsp_kick[i]);
        if (dsp_done[i]) CloseHandle(dsp_done[i]);
    }
    if (factory) factory->lpVtbl->Release(factory);
    if (module) FreeLibrary(module);
    CoUninitialize();
    return status;
}
