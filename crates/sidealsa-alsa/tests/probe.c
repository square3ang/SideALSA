#define _GNU_SOURCE
#include <alsa/asoundlib.h>
#include <errno.h>
#include <inttypes.h>
#include <poll.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <time.h>

/* Private definition: never resolve sidealsa_pro to a physical/default PCM.
 * The daemon must use Q64, 48k and 8 playback channels; this probe requests
 * application B256 independently of the physical buffer.
 * Startup/recovery prefill is not measured as submitted benchmark periods.
 * Installed plugins may differ in prepare/start activation and poll semantics.
 * Counts describe ALSA acceptance, not delivery, latency or daemon HW XRUNs.
 * Blocking writei/handshake has no watchdog; interrupt externally if it hangs.
 */
enum { Q = 64, CHANNELS = 8, BUFFER = 256 };
static int32_t silence[BUFFER * CHANNELS];
static snd_pcm_uframes_t prefill_frames = BUFFER;
static uint64_t calls, prefill_calls, polls, xruns, eagain, errors, recoveries;

static uint64_t now_ns(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) {
        perror("clock_gettime");
        exit(1);
    }
    return (uint64_t)t.tv_sec * 1000000000 + (uint64_t)t.tv_nsec;
}

static double cpu_seconds(const struct rusage *r)
{
    return r->ru_utime.tv_sec + r->ru_stime.tv_sec
        + (r->ru_utime.tv_usec + r->ru_stime.tv_usec) / 1e6;
}

static int prime(snd_pcm_t *pcm)
{
    int err = snd_pcm_prepare(pcm);
    if (err < 0)
        return err;
    for (snd_pcm_uframes_t filled = 0; filled < prefill_frames;) {
        ++prefill_calls;
        snd_pcm_sframes_t n = snd_pcm_writei(pcm, silence, prefill_frames - filled);
        if (n <= 0)
            return n < 0 ? (int)n : -EIO;
        filled += (snd_pcm_uframes_t)n;
    }
    if (snd_pcm_state(pcm) != SND_PCM_STATE_PREPARED)
        return -EBADFD;
    return snd_pcm_start(pcm);
}

static int wait_ready(snd_pcm_t *pcm, struct pollfd *fds, unsigned int count)
{
    for (;;) {
        int err = snd_pcm_poll_descriptors(pcm, fds, count);
        if (err < 0)
            return err;
        if ((unsigned int)err != count)
            return -EIO;
        ++polls;
        err = poll(fds, count, 2000);
        if (err < 0)
            return -errno;
        if (!err)
            return -ETIMEDOUT;
        unsigned short revents = 0;
        err = snd_pcm_poll_descriptors_revents(pcm, fds, count, &revents);
        if (err < 0)
            return err;
        if (revents & (POLLERR | POLLHUP | POLLNVAL)) {
            snd_pcm_sframes_t avail = snd_pcm_avail_update(pcm);
            return avail < 0 ? (int)avail : -EIO;
        }
        if (revents & POLLOUT)
            return 0;
    }
}

static int number(const char *s, uint64_t *out, uint64_t max)
{
    char *end;
    errno = 0;
    if (*s < '0' || *s > '9')
        return -1;
    unsigned long long n = strtoull(s, &end, 10);
    if (errno || *end || n > max)
        return -1;
    *out = n;
    return 0;
}

int main(int argc, char **argv)
{
    uint64_t periods = 7500, work_us = 0, rt = 0, prefill_periods = 4;
    int external_poll = 0;
    if (argc > 6 || (argc > 1 && number(argv[1], &periods, 1000000000))
        || !periods || (argc > 2 && number(argv[2], &work_us, 1000000))
        || (argc > 3 && strcmp(argv[3], "blocking") && strcmp(argv[3], "poll"))
        || (argc > 4 && (number(argv[4], &rt, 46) || (rt != 0 && rt != 46)))
        || (argc > 5 && (number(argv[5], &prefill_periods, 4) || !prefill_periods))) {
        fprintf(stderr, "usage: %s [periods=7500 [work_us=0 [blocking|poll [0|46 [prefill_periods=4]]]]]\n", argv[0]);
        return 2;
    }
    external_poll = argc > 3 && !strcmp(argv[3], "poll");
    prefill_frames = (snd_pcm_uframes_t)prefill_periods * Q;
    snd_pcm_t *pcm = NULL;
    snd_config_t *config = NULL;
    struct pollfd *fds = NULL;
    snd_pcm_hw_params_t *hw;
    snd_pcm_sw_params_t *sw;
    snd_pcm_hw_params_alloca(&hw);
    snd_pcm_sw_params_alloca(&sw);
    int err = 0;
    const char *stage = "configuration";
#define CHECK(expr) do { err = (expr); if (err < 0) { stage = #expr; goto done; } } while (0)
    CHECK(snd_config_load_string(&config,
        "pcm.sidealsa_pro { type sidealsa mode pro socket \"/tmp/sidealsad.sock\" }", 0));
    CHECK(snd_pcm_open_lconf(&pcm, "sidealsa_pro", SND_PCM_STREAM_PLAYBACK,
                            external_poll ? SND_PCM_NONBLOCK : 0, config));
    CHECK(snd_pcm_hw_params_any(pcm, hw));
    CHECK(snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED));
    CHECK(snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S32_LE));
    CHECK(snd_pcm_hw_params_set_channels(pcm, hw, CHANNELS));
    CHECK(snd_pcm_hw_params_set_rate(pcm, hw, 48000, 0));
    CHECK(snd_pcm_hw_params_set_period_size(pcm, hw, Q, 0));
    CHECK(snd_pcm_hw_params_set_buffer_size(pcm, hw, BUFFER));
    CHECK(snd_pcm_hw_params(pcm, hw));
    CHECK(snd_pcm_sw_params_current(pcm, sw));
    snd_pcm_uframes_t boundary;
    CHECK(snd_pcm_sw_params_get_boundary(sw, &boundary));
    CHECK(snd_pcm_sw_params_set_start_threshold(pcm, sw, boundary));
    CHECK(snd_pcm_sw_params_set_stop_threshold(pcm, sw, BUFFER));
    CHECK(snd_pcm_sw_params_set_avail_min(pcm, sw, Q));
    CHECK(snd_pcm_sw_params(pcm, sw));
    int count = snd_pcm_poll_descriptors_count(pcm);
    CHECK(count < 1 ? (count < 0 ? count : -EIO) : 0);
    fds = calloc((size_t)count, sizeof(*fds));
    CHECK(fds ? 0 : -ENOMEM);
    memset(silence, 0, sizeof(silence));
    if (rt) {
        struct sched_param request = { .sched_priority = 46 };
        if (sched_setscheduler(0, SCHED_FIFO, &request))
            fprintf(stderr, "RT46 request failed: %s\n", strerror(errno));
    }
    struct sched_param actual;
    int policy = sched_getscheduler(0);
    CHECK(policy < 0 ? -errno : 0);
    CHECK(sched_getparam(0, &actual) < 0 ? -errno : 0);
    printf("mode=%s rate=48000 period=64 buffer=256 channels=8 format=S32_LE "
           "rt_requested=%" PRIu64 " policy=%d priority=%d rt_verified=%d work_after_poll=%d prefill_periods=%" PRIu64 "\n",
           external_poll ? "poll" : "blocking", rt, policy, actual.sched_priority,
            policy == SCHED_FIFO && actual.sched_priority == 46, external_poll, prefill_periods);
    fflush(stdout);
    struct rusage before, after;
    CHECK(getrusage(RUSAGE_THREAD, &before) < 0 ? -errno : 0);
    uint64_t begin = now_ns(), completed = 0, frames = 0;
    unsigned int offset = 0, consecutive_xruns = 0;
    int work_pending = 1;
    /* Include explicit activation and recovery in CPU/wall cost. */
    stage = "prepare/prefill/start";
    err = prime(pcm);
    while (err >= 0 && completed < periods) {
        stage = "period write/poll";
        if (external_poll)
            err = wait_ready(pcm, fds, (unsigned int)count);
        if (err >= 0) {
            /* Poll-driven backends render after waking, before writing. */
            if (work_pending && work_us) {
                uint64_t until = now_ns() + work_us * 1000;
                while (now_ns() < until) { }
            }
            work_pending = 0;
            ++calls;
            snd_pcm_sframes_t n = snd_pcm_writei(pcm, silence, Q - offset);
            err = n < 0 ? (int)n : n == 0 ? -EIO : 0;
            if (n > 0) {
                frames += (uint64_t)n;
                offset += (unsigned int)n;
                consecutive_xruns = 0;
                if (offset == Q) {
                    ++completed;
                    offset = 0;
                    work_pending = 1;
                }
            }
        }
        if (err == -EAGAIN) {
            ++eagain;
            /* Blocking plugins can also return EAGAIN: avoid a retry spin. */
            err = external_poll ? 0 : wait_ready(pcm, fds, (unsigned int)count);
        }
        if (err == -EPIPE) {
            ++xruns;
            ++errors;
            if (++consecutive_xruns > 100)
                break;
            offset = 0;
            work_pending = 1;
            stage = "recovery prepare/prefill/start";
            err = prime(pcm);
            if (err >= 0)
                ++recoveries;
        }
    }
    double wall = (now_ns() - begin) / 1e9;
    if (getrusage(RUSAGE_THREAD, &after) < 0) {
        err = -errno;
        stage = "getrusage";
        goto done;
    }
    if (err < 0 && consecutive_xruns <= 100) {
        ++errors;
        if (err == -EPIPE) ++xruns;
        if (err == -EAGAIN) ++eagain;
    }
    double cpu = cpu_seconds(&after) - cpu_seconds(&before);
    printf("periods=%" PRIu64 " frames=%" PRIu64 " calls=%" PRIu64
           " prefill_calls=%" PRIu64 " polls=%" PRIu64 " xruns=%" PRIu64
           " eagain=%" PRIu64 " errors=%" PRIu64 " recoveries=%" PRIu64
           " wall_s=%.6f thread_cpu_s=%.6f cpu_pct=%.2f\n",
           completed, frames, calls, prefill_calls, polls, xruns, eagain, errors,
           recoveries, wall, cpu, wall > 0 ? 100 * cpu / wall : 0);
done:
    if (err < 0)
        fprintf(stderr, "%s: %s (%d)\n", stage, snd_strerror(err), err);
    if (pcm) {
        /* Do not drain: this measures submission, not final FIFO delivery. */
        snd_pcm_drop(pcm);
        snd_pcm_close(pcm);
    }
    free(fds);
    if (config) snd_config_delete(config);
    return err < 0 ? 1 : 0;
}
