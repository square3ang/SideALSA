/* Optional syntax/escaping check. Uses the config parser, never a PCM/control device. */
#include <alsa/asoundlib.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv)
{
    snd_config_t *config = NULL, *socket = NULL;
    snd_input_t *input = NULL;
    const char *value = NULL;
    int error;
    if (argc != 3)
        return 2;
    if ((error = snd_config_top(&config)) < 0)
        goto done;
    if ((error = snd_input_stdio_open(&input, argv[1], "r")) < 0)
        goto done;
    if ((error = snd_config_load(config, input)) < 0)
        goto done;
    if ((error = snd_config_search(config, "pcm.sidealsa_pro.socket", &socket)) < 0)
        goto done;
    if ((error = snd_config_get_string(socket, &value)) < 0)
        goto done;
    error = strcmp(value, argv[2]) == 0 ? 0 : -EINVAL;
done:
    if (input)
        snd_input_close(input);
    if (config)
        snd_config_delete(config);
    if (error < 0) {
        fprintf(stderr, "configuration check failed: %s\n", snd_strerror(error));
        return 1;
    }
    puts("PASS: ALSA config parsed; socket roundtrip preserved; no device opened");
    return 0;
}
