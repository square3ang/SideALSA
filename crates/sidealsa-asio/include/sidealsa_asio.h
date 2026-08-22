/* SPDX-License-Identifier: GPL-3.0-or-later */

#pragma once

#include <stdint.h>
#include <windows.h>

typedef struct SideAlsaAsioDriver SideAlsaAsioDriver;

typedef struct
{
    int32_t (*create)(void *context, void **handle, uint32_t *thread_id);
    int32_t (*join)(void *handle);
    uint32_t (*current_thread_id)(void);
} SideAlsaAsioThreadOps;

typedef struct
{
    int32_t is_input_type;
    int32_t channel_number;
    void   *buffers[2];
} SideAlsaAsioBufferInfo;

typedef struct
{
    int32_t channel;
    int32_t is_input;
    int32_t is_active;
    int32_t channel_group;
    int32_t sample_type;
    char    name[32];
} SideAlsaAsioChannelInfo;

typedef struct
{
    uint32_t hi;
    uint32_t lo;
} SideAlsaAsioInt64;

typedef struct
{
    void (CALLBACK *buffer_switch)(int32_t index, int32_t direct_process);
    void (CALLBACK *sample_rate_changed)(double sample_rate);
    int32_t (CALLBACK *asio_message)(int32_t selector, int32_t value, void *message, double *opt);
    void *(CALLBACK *buffer_switch_time_info)(void *time_info, int32_t index,
                                               int32_t direct_process);
} SideAlsaAsioCallbacks;

int32_t sidealsa_asio_new(const SideAlsaAsioThreadOps *thread_ops,
                          SideAlsaAsioDriver **out);
void sidealsa_asio_worker_entry(void *context);
int32_t sidealsa_asio_init(SideAlsaAsioDriver *driver, const char *socket);
int32_t sidealsa_asio_get_channels(SideAlsaAsioDriver *driver, int32_t *inputs, int32_t *outputs);
int32_t sidealsa_asio_get_buffer_size(SideAlsaAsioDriver *driver, int32_t *min_size,
                                      int32_t *max_size, int32_t *preferred_size,
                                      int32_t *granularity);
int32_t sidealsa_asio_create_buffers(SideAlsaAsioDriver *driver, SideAlsaAsioBufferInfo *infos,
                                     int32_t count, int32_t buffer_size,
                                     const SideAlsaAsioCallbacks *callbacks);
int32_t sidealsa_asio_start(SideAlsaAsioDriver *driver);
int32_t sidealsa_asio_stop(SideAlsaAsioDriver *driver);
int32_t sidealsa_asio_dispose_buffers(SideAlsaAsioDriver *driver);
int32_t sidealsa_asio_close(SideAlsaAsioDriver *driver);
int32_t sidealsa_asio_get_channel_info(SideAlsaAsioDriver *driver,
                                       SideAlsaAsioChannelInfo *info);
int32_t sidealsa_asio_get_sample_rate(SideAlsaAsioDriver *driver, double *rate);
int32_t sidealsa_asio_set_sample_rate(SideAlsaAsioDriver *driver, double rate);
int32_t sidealsa_asio_get_latencies(SideAlsaAsioDriver *driver, int32_t *input, int32_t *output);
int32_t sidealsa_asio_get_sample_position(SideAlsaAsioDriver *driver,
                                           SideAlsaAsioInt64 *samples,
                                           SideAlsaAsioInt64 *stamp);
