/* SPDX-License-Identifier: GPL-3.0-or-later */
/* Reuse ABI declarations only; the legacy probe entry point is never invoked. */
#define main sidealsa_unused_legacy_probe_main
#include "asio_probe.c"
#undef main

typedef struct
{
    SideAlsaAsio *asio;
    AsioBufferInfo buffers[4];
    LONG frames, channels;
    LONG count, errors;
    uint64_t position;
    int position_seen, created, started;
} SplitObject;

static SplitObject split[2];

static void split_callback(int object, LONG index)
{
    SplitObject *s = &split[object];
    AsioInt64 position, timestamp;
    if (index < 0 || index > 1)
    {
        InterlockedIncrement(&s->errors);
        return;
    }
    for (LONG channel = 0; channel < s->channels; ++channel)
    {
        float *audio = s->buffers[channel].buffers[index];
        if (!audio)
        {
            InterlockedIncrement(&s->errors);
            continue;
        }
        if (s->buffers[channel].is_input_type)
        {
            for (LONG frame = 0; frame < s->frames; ++frame)
                if (audio[frame] != 0.0f)
                    InterlockedIncrement(&s->errors);
        }
        else
            memset(audio, 0, s->frames * sizeof(*audio));
    }
    if (s->asio->lpVtbl->GetSamplePosition(s->asio, &position, &timestamp) != 0)
        InterlockedIncrement(&s->errors);
    else
    {
        uint64_t value = ((uint64_t)position.hi << 32) | position.lo;
        /* Synthetic scheduling can skip periods, but must not freeze/reverse time. */
        if (s->position_seen && value <= s->position)
            InterlockedIncrement(&s->errors);
        s->position = value;
        s->position_seen = 1;
    }
    InterlockedIncrement(&s->count);
}

static void CALLBACK split_input(LONG index, LONG direct)
{
    (void)direct;
    split_callback(0, index);
}
static void CALLBACK split_output(LONG index, LONG direct)
{
    (void)direct;
    split_callback(1, index);
}
static void *CALLBACK split_input_time(void *info, LONG index, LONG direct)
{
    split_input(index, direct);
    return info;
}
static void *CALLBACK split_output_time(void *info, LONG index, LONG direct)
{
    split_output(index, direct);
    return info;
}
static LONG CALLBACK split_message(LONG selector, LONG value, void *message, double *option)
{
    (void)value;
    (void)message;
    (void)option;
    return selector == 7;
}
static void CALLBACK split_rate(double rate)
{
    if (rate != 48000.0)
        for (int i = 0; i < 2; ++i)
            InterlockedIncrement(&split[i].errors);
}
static AsioCallbacks split_callbacks[2] = {
    { split_input, split_rate, split_message, split_input_time },
    { split_output, split_rate, split_message, split_output_time },
};

/* Each window must deliver several new callbacks, not just an initial Start callback. */
static int split_progress(int mask)
{
    LONG before[2];
    DWORD began = GetTickCount();
    for (int i = 0; i < 2; ++i)
        before[i] = InterlockedCompareExchange(&split[i].count, 0, 0);
    do
    {
        int ready = 1;
        for (int i = 0; i < 2; ++i)
        {
            if (InterlockedCompareExchange(&split[i].errors, 0, 0))
                return 0;
            if ((mask & (1 << i)) &&
                InterlockedCompareExchange(&split[i].count, 0, 0) < before[i] + 20)
                ready = 0;
        }
        if (ready)
            return 1;
        Sleep(5);
    } while (GetTickCount() - began < 5000);
    return 0;
}

static int split_init(IClassFactory *factory, SplitObject *s)
{
    LONG inputs, outputs, minimum, maximum, granularity;
    double rate;
    if (FAILED(IClassFactory_CreateInstance(factory, NULL, &CLSID_SideAlsaAsio,
                                            (void **)&s->asio)) || !s->asio)
        return 0;
    if (!s->asio->lpVtbl->Init(s->asio, NULL) ||
        s->asio->lpVtbl->GetChannels(s->asio, &inputs, &outputs) != 0 ||
        inputs != 2 || outputs != 2 ||
        s->asio->lpVtbl->GetBufferSize(s->asio, &minimum, &maximum, &s->frames,
                                      &granularity) != 0 ||
        s->frames != 64 || minimum > s->frames || maximum < s->frames ||
        s->asio->lpVtbl->GetSampleRate(s->asio, &rate) != 0 || rate != 48000.0)
        return 0;
    return 1;
}

static int split_create(SplitObject *s, int direction, AsioCallbacks *callbacks)
{
    s->channels = direction == 2 ? 4 : 2;
    for (LONG c = 0; c < s->channels; ++c)
    {
        AsioChannelInfo info = { 0 };
        s->buffers[c].is_input_type = direction == 2 ? c < 2 : direction == 0;
        s->buffers[c].channel_number = c % 2;
        info.is_input = s->buffers[c].is_input_type;
        info.channel = s->buffers[c].channel_number;
        if (s->asio->lpVtbl->GetChannelInfo(s->asio, &info) != 0 || info.sample_type != 19)
            return 0; /* ASIOSTFloat32LSB, the format used by these callbacks. */
    }
    if (s->asio->lpVtbl->CreateBuffers(s->asio, s->buffers, s->channels,
                                      s->frames, callbacks) != 0)
        return 0;
    s->created = 1;
    for (LONG c = 0; c < s->channels; ++c)
        for (int b = 0; b < 2; ++b)
        {
            if (!s->buffers[c].buffers[b])
                return 0;
            memset(s->buffers[c].buffers[b], 0, s->frames * sizeof(float));
        }
    return 1;
}

#define SPLIT_CHECK(condition) do { if (!(condition)) { \
    fprintf(stderr, "ASIO split FAIL line=%d: %s\n", __LINE__, #condition); \
    goto cleanup; } } while (0)

int main(void)
{
    HMODULE module = NULL;
    IClassFactory *factory = NULL;
    SplitObject duplicate = { 0 };
    HRESULT (WINAPI *get_factory)(REFCLSID, REFIID, void **);
    int success = 0, initialized = 0;
    char socket[512];
    DWORD socket_length = GetEnvironmentVariableA("SIDEALSA_SOCKET", socket, sizeof(socket));
    SPLIT_CHECK(socket_length > 0 && socket_length < sizeof(socket));
    SPLIT_CHECK(SUCCEEDED(CoInitializeEx(NULL, COINIT_APARTMENTTHREADED)));
    initialized = 1;
    module = LoadLibraryA("sidealsa-asio64.dll");
    SPLIT_CHECK(module);
    get_factory = (void *)GetProcAddress(module, "DllGetClassObject");
    SPLIT_CHECK(get_factory);
    SPLIT_CHECK(SUCCEEDED(get_factory(&CLSID_SideAlsaAsio, &IID_IClassFactory,
                                      (void **)&factory)) && factory);
    for (int first = 0; first < 2; ++first)
    {
        int survivor = 1 - first;
        LONG stopped_count;
        memset(split, 0, sizeof(split));
        /* Both Init calls precede either reservation/CreateBuffers. */
        SPLIT_CHECK(split_init(factory, &split[0]));
        SPLIT_CHECK(split_init(factory, &split[1]));
        SPLIT_CHECK(split[0].asio != split[1].asio);
        SPLIT_CHECK(split_create(&split[first], first, &split_callbacks[first]));
        SPLIT_CHECK(split_create(&split[survivor], survivor, &split_callbacks[survivor]));
        SPLIT_CHECK(split_init(factory, &duplicate));
        for (int direction = 0; direction < 2; ++direction)
        {
            AsioBufferInfo buffer = { .is_input_type = direction == 0, .channel_number = 0 };
            LONG result = duplicate.asio->lpVtbl->CreateBuffers(
                duplicate.asio, &buffer, 1, duplicate.frames, &split_callbacks[direction]);
            duplicate.created = result == 0;
            SPLIT_CHECK(result != 0);
        }
        duplicate.asio->lpVtbl->Release(duplicate.asio);
        memset(&duplicate, 0, sizeof(duplicate));
        SPLIT_CHECK(split[first].asio->lpVtbl->Start(split[first].asio) == 0);
        split[first].started = 1;
        SPLIT_CHECK(split_progress(1 << first));
        SPLIT_CHECK(split[survivor].asio->lpVtbl->Start(split[survivor].asio) == 0);
        split[survivor].started = 1;
        SPLIT_CHECK(split_progress(3));
        SPLIT_CHECK(split_progress(3));
        SPLIT_CHECK(split[first].asio->lpVtbl->Stop(split[first].asio) == 0);
        split[first].started = 0;
        stopped_count = InterlockedCompareExchange(&split[first].count, 0, 0);
        SPLIT_CHECK(split_progress(1 << survivor));
        SPLIT_CHECK(split[first].asio->lpVtbl->DisposeBuffers(split[first].asio) == 0);
        split[first].created = 0;
        SPLIT_CHECK(split_progress(1 << survivor));
        split[first].asio->lpVtbl->Release(split[first].asio);
        split[first].asio = NULL;
        SPLIT_CHECK(split_progress(1 << survivor));
        SPLIT_CHECK(InterlockedCompareExchange(&split[first].count, 0, 0) == stopped_count);
        SPLIT_CHECK(split[survivor].asio->lpVtbl->Stop(split[survivor].asio) == 0);
        split[survivor].started = 0;
        SPLIT_CHECK(InterlockedCompareExchange(&split[survivor].errors, 0, 0) == 0);
        SPLIT_CHECK(split[survivor].asio->lpVtbl->DisposeBuffers(split[survivor].asio) == 0);
        split[survivor].created = 0;
        split[survivor].asio->lpVtbl->Release(split[survivor].asio);
        split[survivor].asio = NULL;
        fprintf(stderr, "ASIO split order=%s-first input_callbacks=%ld output_callbacks=%ld\n",
                first == 0 ? "input" : "output", (long)split[0].count, (long)split[1].count);
        memset(split, 0, sizeof(split));
        SPLIT_CHECK(split_init(factory, &split[0]));
        SPLIT_CHECK(split_create(&split[0], 2, &split_callbacks[0]));
        SPLIT_CHECK(split[0].asio->lpVtbl->Start(split[0].asio) == 0);
        split[0].started = 1;
        SPLIT_CHECK(split_progress(1));
        SPLIT_CHECK(split[0].asio->lpVtbl->Stop(split[0].asio) == 0);
        split[0].started = 0;
        SPLIT_CHECK(InterlockedCompareExchange(&split[0].errors, 0, 0) == 0);
        SPLIT_CHECK(split[0].asio->lpVtbl->DisposeBuffers(split[0].asio) == 0);
        split[0].created = 0;
        split[0].asio->lpVtbl->Release(split[0].asio);
        split[0].asio = NULL;
        fprintf(stderr, "ASIO split fresh duplex PASS\n");
    }
    success = 1;
cleanup:
    for (int i = 0; i < 3; ++i)
    {
        SplitObject *s = i == 2 ? &duplicate : &split[i];
        if (!s->asio)
            continue;
        if (s->started)
            s->asio->lpVtbl->Stop(s->asio);
        if (s->created)
            s->asio->lpVtbl->DisposeBuffers(s->asio);
        s->asio->lpVtbl->Release(s->asio);
    }
    if (factory)
        IClassFactory_Release(factory);
    if (module)
        FreeLibrary(module);
    if (initialized)
        CoUninitialize();
    fprintf(stderr, "ASIO split %s\n", success ? "PASS" : "FAIL");
    return success ? 0 : 1;
}
