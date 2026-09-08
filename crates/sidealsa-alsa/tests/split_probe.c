/* Hardware-free fake-daemon probe. No ALSA config installation is needed.
 * Standalone: cc split_probe.c -lasound -ldl -o split-probe
 * Run: split-probe /absolute/plugin.so /path/to/fake.sock
 * The Rust test loads this as a library and supplies the linked plugin entry.
 */
#include <alsa/asoundlib.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef int (*plugin_open_t)(snd_pcm_t **, const char *, snd_config_t *,
                            snd_config_t *, snd_pcm_stream_t, int);
typedef void (*owner_check_t)(const char *, int);
#define CHECK(expr) do { if (!(expr)) { fprintf(stderr, "probe line %d: %s\n", \
    __LINE__, #expr); return __LINE__; } } while (0)

static int configure(snd_pcm_t *pcm)
{
    snd_pcm_hw_params_t *hw;
    snd_pcm_hw_params_alloca(&hw);
    CHECK(snd_pcm_hw_params_any(pcm, hw) >= 0);
    CHECK(snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED) == 0);
    CHECK(snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S32_LE) == 0);
    CHECK(snd_pcm_hw_params_set_channels(pcm, hw, 1) == 0);
    CHECK(snd_pcm_hw_params_set_rate(pcm, hw, 48000, 0) == 0);
    CHECK(snd_pcm_hw_params_set_period_size(pcm, hw, 64, 0) == 0);
    CHECK(snd_pcm_hw_params_set_buffer_size(pcm, hw, 128) == 0);
    CHECK(snd_pcm_hw_params(pcm, hw) == 0);
    snd_pcm_sw_params_t *sw;
    snd_pcm_sw_params_alloca(&sw);
    CHECK(snd_pcm_sw_params_current(pcm, sw) == 0);
    CHECK(snd_pcm_sw_params_set_start_threshold(pcm, sw, 128) == 0);
    CHECK(snd_pcm_sw_params(pcm, sw) == 0);
    CHECK(snd_pcm_prepare(pcm) == 0);
    return 0;
}

static int read_prefix(snd_pcm_t *capture, int first)
{
    int32_t samples[16];
    CHECK(snd_pcm_readi(capture, samples, 16) == 16);
    for (int i = 0; i < 16; ++i)
        CHECK(samples[i] == first + i);
    return 0;
}

int sidealsa_split_probe(plugin_open_t open_plugin, const char *socket,
                        int capture_first, owner_check_t other_owner)
{
    snd_config_t *conf, *node;
    snd_pcm_t *pcm[2] = { NULL, NULL }, *duplicate = NULL;
    CHECK(snd_config_top(&conf) == 0);
    CHECK(snd_config_imake_string(&node, "socket", socket) == 0);
    CHECK(snd_config_add(conf, node) == 0);
    int first = capture_first ? 1 : 0;
    CHECK(open_plugin(&pcm[first], "sidealsa_pro", conf, conf,
                      first, SND_PCM_NONBLOCK) == 0);
    if (other_owner)
        other_owner(socket, 1 - first);
    CHECK(open_plugin(&pcm[1 - first], "sidealsa_pro", conf, conf,
                      1 - first, SND_PCM_NONBLOCK) == 0);
    for (int i = 0; i < 2; ++i) {
        CHECK(open_plugin(&duplicate, "sidealsa_pro", conf, conf,
                          i, SND_PCM_NONBLOCK) == -EBUSY);
        CHECK(configure(pcm[i]) == 0);
    }
    struct pollfd p[2], c[2];
    CHECK(snd_pcm_poll_descriptors(pcm[0], p, 2) == 2);
    CHECK(snd_pcm_poll_descriptors(pcm[1], c, 2) == 2);
    CHECK(p[0].fd != c[0].fd && p[1].fd != c[1].fd);
    CHECK(snd_pcm_start(pcm[1]) == 0);
    int32_t output[64] = { 42 };
    CHECK(snd_pcm_writei(pcm[0], output, 64) == 64);
    CHECK(snd_pcm_start(pcm[0]) == 0);
    /* Playback wait/submit must not drain or consume the capture endpoint. */
    CHECK(poll(c, 2, 0) == 1 && (c[0].revents & POLLIN));
    CHECK(poll(p, 2, 0) == 0);
    CHECK(read_prefix(pcm[1], 0) == 0);
    CHECK(snd_pcm_drop(pcm[0]) == 0);
    CHECK(snd_pcm_prepare(pcm[0]) == 0);
    CHECK(read_prefix(pcm[1], 16) == 0);
    CHECK(snd_pcm_writei(pcm[0], output, 64) == 64);
    CHECK(snd_pcm_start(pcm[0]) == 0);
    CHECK(snd_pcm_drop(pcm[1]) == 0);
    CHECK(snd_pcm_prepare(pcm[1]) == 0);
    CHECK(snd_pcm_state(pcm[0]) == SND_PCM_STATE_RUNNING);
    CHECK(snd_pcm_start(pcm[1]) == 0);
    CHECK(read_prefix(pcm[1], 100) == 0);
    /* Close either direction first; the survivor still transfers real data. */
    CHECK(snd_pcm_close(pcm[first]) == 0);
    if (first == 0)
        CHECK(read_prefix(pcm[1], 116) == 0);
    else
        CHECK(snd_pcm_writei(pcm[0], output, 64) == 64);
    CHECK(snd_pcm_close(pcm[1 - first]) == 0);
    snd_config_delete(conf);
    return 0;
}

#ifndef SIDEALSA_PROBE_LIBRARY
int main(int argc, char **argv)
{
    CHECK(argc == 3);
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    CHECK(library != NULL);
    plugin_open_t open_plugin = (plugin_open_t)dlsym(library, "_snd_pcm_sidealsa_open");
    CHECK(open_plugin != NULL);
    CHECK(sidealsa_split_probe(open_plugin, argv[2], 0, NULL) == 0);
    CHECK(sidealsa_split_probe(open_plugin, argv[2], 1, NULL) == 0);
    dlclose(library);
    return 0;
}
#endif
