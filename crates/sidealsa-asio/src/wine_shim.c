/* SPDX-License-Identifier: GPL-3.0-or-later */

#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "objbase.h"
#include "winreg.h"

#include "sidealsa_asio.h"

#define SIDEALSA_ASIO_CLSID_STRING "{8C4D6A10-5A7D-4CC2-AE13-7D9E3E2A1B40}"
#define SIDEALSA_ASIO_PROGID "SideALSA"
#define SIDEALSA_ASIO_NAME "SideALSA ASIO"

static const CLSID CLSID_SideAlsaAsio
        = { 0x8c4d6a10, 0x5a7d, 0x4cc2, { 0xae, 0x13, 0x7d, 0x9e, 0x3e, 0x2a, 0x1b, 0x40 } };

typedef struct SideAlsaAsio SideAlsaAsio;
typedef struct SideAlsaAsioVtbl SideAlsaAsioVtbl;

struct SideAlsaAsio
{
    const SideAlsaAsioVtbl *lpVtbl;
    LONG                    ref;
    SideAlsaAsioDriver     *driver;
    HMODULE                 cleanup_module;
};

struct SideAlsaAsioVtbl
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
    LONG(WINAPI *GetSamplePosition)(SideAlsaAsio *, SideAlsaAsioInt64 *, SideAlsaAsioInt64 *);
    LONG(WINAPI *GetChannelInfo)(SideAlsaAsio *, SideAlsaAsioChannelInfo *);
    LONG(WINAPI *CreateBuffers)(SideAlsaAsio *, SideAlsaAsioBufferInfo *, LONG, LONG,
                                SideAlsaAsioCallbacks *);
    LONG(WINAPI *DisposeBuffers)(SideAlsaAsio *);
    LONG(WINAPI *ControlPanel)(SideAlsaAsio *);
    LONG(WINAPI *Future)(SideAlsaAsio *, LONG, void *);
    LONG(WINAPI *OutputReady)(SideAlsaAsio *);
};

static LONG driver_objects;
static LONG server_locks;

static DWORD WINAPI
sidealsa_worker_trampoline(void *context)
{
    sidealsa_asio_worker_entry(context);
    return 0;
}

static int32_t
sidealsa_worker_create(void *context, void **handle, uint32_t *thread_id)
{
    HANDLE thread;

    if (!context || !handle || !thread_id)
        return ERROR_INVALID_PARAMETER;
    thread = CreateThread(NULL, 8 * 1024 * 1024, sidealsa_worker_trampoline, context,
                          STACK_SIZE_PARAM_IS_A_RESERVATION | CREATE_SUSPENDED,
                          (DWORD *)thread_id);
    if (!thread)
        return (int32_t)GetLastError();
    if (!SetThreadPriority(thread, THREAD_PRIORITY_TIME_CRITICAL))
        SetThreadPriority(thread, THREAD_PRIORITY_HIGHEST);
    if (ResumeThread(thread) == (DWORD)-1) {
        DWORD error = GetLastError();
        TerminateThread(thread, error);
        CloseHandle(thread);
        return (int32_t)error;
    }
    *handle = thread;
    return 0;
}

static int32_t
sidealsa_worker_join(void *handle, uint32_t timeout_ms)
{
    DWORD result;

    if (!handle)
        return ERROR_INVALID_HANDLE;
    result = WaitForSingleObject(handle, timeout_ms);
    if (result == WAIT_TIMEOUT)
        return ERROR_TIMEOUT;
    if (result != WAIT_OBJECT_0)
        return (int32_t)GetLastError();
    if (!CloseHandle(handle))
        return (int32_t)GetLastError();
    return 0;
}

static uint32_t
sidealsa_current_thread_id(void)
{
    return GetCurrentThreadId();
}

static const SideAlsaAsioThreadOps sidealsa_thread_ops = {
    sidealsa_worker_create,
    sidealsa_worker_join,
    sidealsa_current_thread_id,
};

static HRESULT WINAPI asio_query_interface(SideAlsaAsio *, REFIID, void **);
static ULONG WINAPI asio_add_ref(SideAlsaAsio *);
static ULONG WINAPI asio_release(SideAlsaAsio *);
static LONG WINAPI asio_init(SideAlsaAsio *, void *);
static void WINAPI asio_get_driver_name(SideAlsaAsio *, char *);
static LONG WINAPI asio_get_driver_version(SideAlsaAsio *);
static void WINAPI asio_get_error_message(SideAlsaAsio *, char *);
static LONG WINAPI asio_start(SideAlsaAsio *);
static LONG WINAPI asio_stop(SideAlsaAsio *);
static LONG WINAPI asio_get_channels(SideAlsaAsio *, LONG *, LONG *);
static LONG WINAPI asio_get_latencies(SideAlsaAsio *, LONG *, LONG *);
static LONG WINAPI asio_get_buffer_size(SideAlsaAsio *, LONG *, LONG *, LONG *, LONG *);
static LONG WINAPI asio_can_sample_rate(SideAlsaAsio *, double);
static LONG WINAPI asio_get_sample_rate(SideAlsaAsio *, double *);
static LONG WINAPI asio_set_sample_rate(SideAlsaAsio *, double);
static LONG WINAPI asio_get_clock_sources(SideAlsaAsio *, void *, LONG *);
static LONG WINAPI asio_set_clock_source(SideAlsaAsio *, LONG);
static LONG WINAPI asio_get_sample_position(SideAlsaAsio *, SideAlsaAsioInt64 *,
                                            SideAlsaAsioInt64 *);
static LONG WINAPI asio_get_channel_info(SideAlsaAsio *, SideAlsaAsioChannelInfo *);
static LONG WINAPI asio_create_buffers(SideAlsaAsio *, SideAlsaAsioBufferInfo *, LONG, LONG,
                                       SideAlsaAsioCallbacks *);
static LONG WINAPI asio_dispose_buffers(SideAlsaAsio *);
static LONG WINAPI asio_control_panel(SideAlsaAsio *);
static LONG WINAPI asio_future(SideAlsaAsio *, LONG, void *);
static LONG WINAPI asio_output_ready(SideAlsaAsio *);

static const SideAlsaAsioVtbl asio_vtbl = {
    asio_query_interface,
    asio_add_ref,
    asio_release,
    asio_init,
    asio_get_driver_name,
    asio_get_driver_version,
    asio_get_error_message,
    asio_start,
    asio_stop,
    asio_get_channels,
    asio_get_latencies,
    asio_get_buffer_size,
    asio_can_sample_rate,
    asio_get_sample_rate,
    asio_set_sample_rate,
    asio_get_clock_sources,
    asio_set_clock_source,
    asio_get_sample_position,
    asio_get_channel_info,
    asio_create_buffers,
    asio_dispose_buffers,
    asio_control_panel,
    asio_future,
    asio_output_ready,
};

static HRESULT WINAPI
asio_query_interface(SideAlsaAsio *self, REFIID riid, void **out)
{
    if (!out)
        return E_POINTER;
    *out = NULL;
    if (!IsEqualIID(riid, &IID_IUnknown) && !IsEqualGUID(riid, &CLSID_SideAlsaAsio))
        return E_NOINTERFACE;
    asio_add_ref(self);
    *out = self;
    return S_OK;
}

static ULONG WINAPI
asio_add_ref(SideAlsaAsio *self)
{
    return (ULONG)InterlockedIncrement(&self->ref);
}

static DWORD WINAPI
asio_deferred_release(void *context)
{
    SideAlsaAsio *self = context;
    HMODULE module = self->cleanup_module;

    while (self->driver && sidealsa_asio_close(self->driver) != 0)
        Sleep(10);
    self->driver = NULL;
    InterlockedDecrement(&driver_objects);
    HeapFree(GetProcessHeap(), 0, self);
    if (module)
        FreeLibraryAndExitThread(module, 0);
    return 0;
}

static ULONG WINAPI
asio_release(SideAlsaAsio *self)
{
    LONG ref = InterlockedDecrement(&self->ref);
    if (ref == 0)
    {
        if (self->driver)
        {
            HANDLE cleanup;
            HMODULE module = NULL;

            if (!GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                                    (LPCSTR)(uintptr_t)asio_deferred_release, &module))
                return 0;
            self->cleanup_module = module;
            cleanup = CreateThread(NULL, 0, asio_deferred_release, self, 0, NULL);
            if (!cleanup)
                return 0;
            CloseHandle(cleanup);
            return 0;
        }
        InterlockedDecrement(&driver_objects);
        HeapFree(GetProcessHeap(), 0, self);
    }
    return (ULONG)ref;
}

static const char *
asio_socket_path(char *path, size_t size)
{
    DWORD length = GetEnvironmentVariableA("SIDEALSA_SOCKET", path, (DWORD)size);
    if (!length || length >= size)
        return "/tmp/sidealsad.sock";
    return path;
}

static LONG WINAPI
asio_init(SideAlsaAsio *self, void *sys_ref)
{
    char path[256];
    LONG result;
    (void)sys_ref;
    if (!self->driver)
    {
        if (sidealsa_asio_new(&sidealsa_thread_ops, &self->driver) != 0)
            return 0;
    }
    result = sidealsa_asio_init(self->driver, asio_socket_path(path, sizeof(path)));
    return result == 0 ? 1 : 0;
}

static void WINAPI
asio_get_driver_name(SideAlsaAsio *self, char *name)
{
    (void)self;
    if (name)
        lstrcpynA(name, SIDEALSA_ASIO_NAME, 32);
}

static LONG WINAPI
asio_get_driver_version(SideAlsaAsio *self)
{
    (void)self;
    return 1;
}

static void WINAPI
asio_get_error_message(SideAlsaAsio *self, char *message)
{
    if (message && self->driver
        && sidealsa_asio_get_error_message(self->driver, message, 124) == 0)
        return;
    if (message)
        lstrcpynA(message, "SideALSA operation failed; check sidealsad", 124);
}

static LONG WINAPI
asio_start(SideAlsaAsio *self)
{
    return self->driver ? sidealsa_asio_start(self->driver) : -1000;
}

static LONG WINAPI
asio_stop(SideAlsaAsio *self)
{
    return self->driver ? sidealsa_asio_stop(self->driver) : -1000;
}

static LONG WINAPI
asio_get_channels(SideAlsaAsio *self, LONG *inputs, LONG *outputs)
{
    return self->driver ? sidealsa_asio_get_channels(self->driver, inputs, outputs) : -1000;
}

static LONG WINAPI
asio_get_latencies(SideAlsaAsio *self, LONG *input, LONG *output)
{
    return self->driver ? sidealsa_asio_get_latencies(self->driver, input, output) : -1000;
}

static LONG WINAPI
asio_get_buffer_size(SideAlsaAsio *self, LONG *min_size, LONG *max_size, LONG *preferred_size,
                     LONG *granularity)
{
    return self->driver
                   ? sidealsa_asio_get_buffer_size(self->driver, min_size, max_size,
                                                   preferred_size, granularity)
                   : -1000;
}

static LONG WINAPI
asio_can_sample_rate(SideAlsaAsio *self, double rate)
{
    double current;
    LONG   result;
    if (!self->driver)
        return -1000;
    result = sidealsa_asio_get_sample_rate(self->driver, &current);
    if (result != 0)
        return result;
    return current == rate ? 0 : -995;
}

static LONG WINAPI
asio_get_sample_rate(SideAlsaAsio *self, double *rate)
{
    return self->driver ? sidealsa_asio_get_sample_rate(self->driver, rate) : -1000;
}

static LONG WINAPI
asio_set_sample_rate(SideAlsaAsio *self, double rate)
{
    return self->driver ? sidealsa_asio_set_sample_rate(self->driver, rate) : -1000;
}

typedef struct
{
    LONG index;
    LONG associated_channel;
    LONG associated_group;
    LONG is_current_source;
    char name[32];
} SideAlsaClockSource;

static LONG WINAPI
asio_get_clock_sources(SideAlsaAsio *self, void *clocks, LONG *count)
{
    SideAlsaClockSource *source;
    LONG                 capacity;
    if (!count)
        return -998;
    capacity = *count;
    if (capacity < 0 || (capacity > 0 && !clocks))
        return -998;
    *count = 1;
    if (!capacity)
        return 0;
    source = clocks;
    memset(source, 0, sizeof(*source));
    source->associated_channel = -1;
    source->associated_group   = -1;
    source->is_current_source  = 1;
    lstrcpynA(source->name, "Internal", sizeof(source->name));
    return 0;
}

static LONG WINAPI
asio_set_clock_source(SideAlsaAsio *self, LONG index)
{
    return self->driver && index == 0 ? 0 : -1000;
}

static LONG WINAPI
asio_get_sample_position(SideAlsaAsio *self, SideAlsaAsioInt64 *samples,
                         SideAlsaAsioInt64 *stamp)
{
    return self->driver ? sidealsa_asio_get_sample_position(self->driver, samples, stamp) : -1000;
}

static LONG WINAPI
asio_get_channel_info(SideAlsaAsio *self, SideAlsaAsioChannelInfo *info)
{
    return self->driver ? sidealsa_asio_get_channel_info(self->driver, info) : -1000;
}

static LONG WINAPI
asio_create_buffers(SideAlsaAsio *self, SideAlsaAsioBufferInfo *infos, LONG count, LONG size,
                     SideAlsaAsioCallbacks *callbacks)
{
    LONG result;

    if (!self->driver)
        return -1000;
    asio_add_ref(self);
    result = sidealsa_asio_create_buffers(self->driver, infos, count, size, callbacks);
    asio_release(self);
    return result;
}

static LONG WINAPI
asio_dispose_buffers(SideAlsaAsio *self)
{
    return self->driver ? sidealsa_asio_dispose_buffers(self->driver) : -1000;
}

static LONG WINAPI
asio_control_panel(SideAlsaAsio *self)
{
    (void)self;
    return -1000;
}

static LONG WINAPI
asio_future(SideAlsaAsio *self, LONG selector, void *option)
{
    (void)self;
    (void)option;
    return selector == 10 ? 0x3f4847a0 : -998;
}

static LONG WINAPI
asio_output_ready(SideAlsaAsio *self)
{
    (void)self;
    return -1000;
}

typedef struct
{
    const IClassFactoryVtbl *lpVtbl;
    LONG                     ref;
} SideAlsaClassFactory;

static ULONG WINAPI
factory_add_ref(LPCLASSFACTORY iface)
{
    SideAlsaClassFactory *factory = (SideAlsaClassFactory *)iface;
    return (ULONG)InterlockedIncrement(&factory->ref);
}

static HRESULT WINAPI
factory_query_interface(LPCLASSFACTORY iface, REFIID riid, void **out)
{
    if (!out)
        return E_POINTER;
    *out = NULL;
    if (!IsEqualIID(riid, &IID_IUnknown) && !IsEqualIID(riid, &IID_IClassFactory))
        return E_NOINTERFACE;
    factory_add_ref(iface);
    *out = iface;
    return S_OK;
}

static ULONG WINAPI
factory_release(LPCLASSFACTORY iface)
{
    SideAlsaClassFactory *factory = (SideAlsaClassFactory *)iface;
    LONG                   ref     = InterlockedDecrement(&factory->ref);
    return (ULONG)ref;
}

static HRESULT WINAPI
factory_create_instance(LPCLASSFACTORY iface, LPUNKNOWN outer, REFIID riid, void **out)
{
    SideAlsaAsio *object;
    HRESULT       result;
    (void)iface;
    if (outer)
        return CLASS_E_NOAGGREGATION;
    if (!out)
        return E_POINTER;
    *out = NULL;
    object = HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(*object));
    if (!object)
        return E_OUTOFMEMORY;
    object->lpVtbl = &asio_vtbl;
    object->ref    = 1;
    InterlockedIncrement(&driver_objects);
    result = asio_query_interface(object, riid, out);
    asio_release(object);
    return result;
}

static HRESULT WINAPI
factory_lock_server(LPCLASSFACTORY iface, BOOL lock)
{
    (void)iface;
    if (lock)
        InterlockedIncrement(&server_locks);
    else
        InterlockedDecrement(&server_locks);
    return S_OK;
}

static const IClassFactoryVtbl factory_vtbl = {
    factory_query_interface,
    factory_add_ref,
    factory_release,
    factory_create_instance,
    factory_lock_server,
};

static SideAlsaClassFactory factory = { &factory_vtbl, 1 };

HRESULT WINAPI
DllGetClassObject(REFCLSID clsid, REFIID riid, void **out)
{
    if (!out)
        return E_POINTER;
    *out = NULL;
    if (!IsEqualGUID(clsid, &CLSID_SideAlsaAsio))
        return CLASS_E_CLASSNOTAVAILABLE;
    return factory_query_interface((LPCLASSFACTORY)&factory, riid, out);
}

HRESULT WINAPI
DllCanUnloadNow(void)
{
    return InterlockedCompareExchange(&driver_objects, 0, 0) == 0
                    && InterlockedCompareExchange(&server_locks, 0, 0) == 0
                ? S_OK
                : S_FALSE;
}

static LONG
set_registry_value(HKEY root, const char *path, const char *name, const char *value)
{
    HKEY key;
    LONG result = RegCreateKeyExA(root, path, 0, NULL, 0, KEY_READ | KEY_WRITE, NULL, &key, NULL);
    if (result != ERROR_SUCCESS)
        return result;
    result = RegSetValueExA(key, name, 0, REG_SZ, (const BYTE *)value, (DWORD)strlen(value) + 1);
    RegCloseKey(key);
    return result;
}

HRESULT WINAPI
DllRegisterServer(void)
{
    char clsid_path[128];
    HRESULT result;
    snprintf(clsid_path, sizeof(clsid_path), "CLSID\\%s\\InprocServer32",
             SIDEALSA_ASIO_CLSID_STRING);
    result = HRESULT_FROM_WIN32(set_registry_value(HKEY_CLASSES_ROOT, clsid_path, NULL,
                                                   "sidealsa-asio64.dll"));
    if (FAILED(result))
        return result;
    result = HRESULT_FROM_WIN32(set_registry_value(HKEY_CLASSES_ROOT, clsid_path,
                                                   "ThreadingModel", "Apartment"));
    if (FAILED(result))
        return result;
    result = HRESULT_FROM_WIN32(set_registry_value(HKEY_LOCAL_MACHINE,
                                                   "Software\\ASIO\\SideALSA", "CLSID",
                                                   SIDEALSA_ASIO_CLSID_STRING));
    if (FAILED(result))
        return result;
    return HRESULT_FROM_WIN32(set_registry_value(HKEY_LOCAL_MACHINE,
                                                  "Software\\ASIO\\SideALSA", "Description",
                                                  SIDEALSA_ASIO_NAME));
}

HRESULT WINAPI
DllUnregisterServer(void)
{
    char clsid_path[128];
    snprintf(clsid_path, sizeof(clsid_path), "CLSID\\%s",
             SIDEALSA_ASIO_CLSID_STRING);
    RegDeleteTreeA(HKEY_CLASSES_ROOT, clsid_path);
    RegDeleteTreeA(HKEY_LOCAL_MACHINE, "Software\\ASIO\\SideALSA");
    return S_OK;
}

BOOL WINAPI
DllMain(HINSTANCE instance, DWORD reason, LPVOID reserved)
{
    (void)instance;
    (void)reason;
    (void)reserved;
    return TRUE;
}
