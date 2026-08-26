#define _GNU_SOURCE

#include <alsa/asoundlib.h>
#include <alsa/pcm_external.h>
#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

typedef struct sidealsa_stream sidealsa_stream_t;

extern int sidealsa_stream_open(const char *socket, int mode, const char *port_id,
                                int direction, int nonblock, sidealsa_stream_t **stream,
                                unsigned int *rate, unsigned int *channels,
                                unsigned int *period, unsigned int *minimum_buffer,
                                unsigned int *buffer, int *poll_fd, int *control_fd);
extern int sidealsa_stream_start(sidealsa_stream_t *stream);
extern int sidealsa_stream_stop(sidealsa_stream_t *stream);
extern int sidealsa_stream_prepare(sidealsa_stream_t *stream);
extern int sidealsa_stream_set_nonblock(sidealsa_stream_t *stream, int nonblock);
extern int sidealsa_stream_drain(sidealsa_stream_t *stream);
extern ssize_t sidealsa_stream_transfer(sidealsa_stream_t *stream,
                                        const snd_pcm_channel_area_t *areas,
                                        size_t offset, size_t frames);
extern uint64_t sidealsa_stream_position(const sidealsa_stream_t *stream);
extern int sidealsa_stream_close(sidealsa_stream_t *stream);

typedef struct {
	snd_pcm_ioplug_t io;
	sidealsa_stream_t *stream;
	int poll_fd;
	int control_fd;
	unsigned int rate;
	unsigned int channels;
	snd_pcm_uframes_t period_size;
	snd_pcm_uframes_t minimum_buffer_size;
	snd_pcm_uframes_t buffer_size;
	snd_pcm_uframes_t boundary;
	int shared;
} sidealsa_pcm_t;

static void sidealsa_set_error_state(snd_pcm_ioplug_t *io, long result)
{
	if (result == -ENODEV)
		snd_pcm_ioplug_set_state(io, SND_PCM_STATE_DISCONNECTED);
	else if (result == -EPIPE)
		snd_pcm_ioplug_set_state(io, SND_PCM_STATE_XRUN);
}

static int sidealsa_sync_nonblock(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int result = sidealsa_stream_set_nonblock(pcm->stream, io->nonblock);

	sidealsa_set_error_state(io, result);
	return result;
}

static int sidealsa_start(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int result = sidealsa_sync_nonblock(io);

	if (result >= 0)
		result = sidealsa_stream_start(pcm->stream);
	sidealsa_set_error_state(io, result);
	return result;
}

static int sidealsa_stop(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int result = sidealsa_stream_stop(pcm->stream);

	sidealsa_set_error_state(io, result);
	return result;
}

static int sidealsa_prepare(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int result = sidealsa_sync_nonblock(io);

	if (result >= 0)
		result = sidealsa_stream_prepare(pcm->stream);
	sidealsa_set_error_state(io, result);
	return result;
}

static int sidealsa_drain(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int result = sidealsa_sync_nonblock(io);

	if (result >= 0)
		result = sidealsa_stream_drain(pcm->stream);
	sidealsa_set_error_state(io, result);
	return result;
}

static snd_pcm_sframes_t sidealsa_transfer(snd_pcm_ioplug_t *io,
						   const snd_pcm_channel_area_t *areas,
						   snd_pcm_uframes_t offset,
						   snd_pcm_uframes_t size)
{
	sidealsa_pcm_t *pcm = io->private_data;
	snd_pcm_sframes_t result = sidealsa_sync_nonblock(io);

	if (result >= 0)
		result = sidealsa_stream_transfer(pcm->stream, areas, offset, size);
	sidealsa_set_error_state(io, result);
	return result;
}

static snd_pcm_sframes_t sidealsa_pointer(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	uint64_t position = sidealsa_stream_position(pcm->stream);
	snd_pcm_uframes_t boundary = pcm->boundary ? pcm->boundary : io->buffer_size;
	if (!boundary)
		return 0;
	snd_pcm_uframes_t hw_ptr = position % boundary;
	if (io->state == SND_PCM_STATE_RUNNING &&
	    snd_pcm_ioplug_avail(io, hw_ptr, io->appl_ptr) > io->buffer_size)
		return -EPIPE;
	return (snd_pcm_sframes_t)hw_ptr;
}

static int sidealsa_sw_params(snd_pcm_ioplug_t *io, snd_pcm_sw_params_t *params)
{
	sidealsa_pcm_t *pcm = io->private_data;
	return snd_pcm_sw_params_get_boundary(params, &pcm->boundary);
}

static int sidealsa_poll_revents(snd_pcm_ioplug_t *io, struct pollfd *pfds,
					 unsigned int nfds, unsigned short *revents)
{
	if (nfds != 2 || !pfds || !revents)
		return -EINVAL;
	if (pfds[1].revents & (POLLRDHUP | POLLHUP | POLLERR | POLLNVAL)) {
		snd_pcm_ioplug_set_state(io, SND_PCM_STATE_DISCONNECTED);
		*revents = POLLERR;
		return 0;
	}
	*revents = (pfds[0].revents | pfds[1].revents) & (POLLERR | POLLNVAL);
	if (pfds[0].revents & POLLIN)
		*revents |= io->stream == SND_PCM_STREAM_PLAYBACK ? POLLOUT : POLLIN;
	return 0;
}

static int sidealsa_poll_descriptors_count(snd_pcm_ioplug_t *io)
{
	(void)io;
	return 2;
}

static int sidealsa_poll_descriptors(snd_pcm_ioplug_t *io, struct pollfd *pfds,
				     unsigned int space)
{
	sidealsa_pcm_t *pcm = io->private_data;

	if (!pfds || space < 2)
		return -EINVAL;
	pfds[0].fd = pcm->poll_fd;
	pfds[0].events = POLLIN;
	pfds[0].revents = 0;
	pfds[1].fd = pcm->control_fd;
	pfds[1].events = POLLRDHUP;
	pfds[1].revents = 0;
	return 2;
}

static int sidealsa_close(snd_pcm_ioplug_t *io)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int result = sidealsa_stream_close(pcm->stream);
	if (pcm->poll_fd >= 0)
		close(pcm->poll_fd);
	if (pcm->control_fd >= 0)
		close(pcm->control_fd);
	free(pcm);
	return result;
}

static int sidealsa_hw_params(snd_pcm_ioplug_t *io, snd_pcm_hw_params_t *params)
{
	sidealsa_pcm_t *pcm = io->private_data;
	int valid_buffer;

	(void)params;
	valid_buffer = pcm->shared ?
		io->buffer_size >= pcm->minimum_buffer_size &&
		io->buffer_size <= pcm->buffer_size &&
		io->buffer_size % pcm->period_size == 0 :
		io->buffer_size == pcm->buffer_size;
	if (io->format != SND_PCM_FORMAT_S32_LE ||
	    io->channels != pcm->channels ||
	    io->rate != pcm->rate ||
	    io->period_size != pcm->period_size ||
	    !valid_buffer)
		return -EINVAL;
	return 0;
}

static int sidealsa_set_constraints(sidealsa_pcm_t *pcm)
{
	snd_pcm_ioplug_t *io = &pcm->io;
	static const unsigned int access[] = { SND_PCM_ACCESS_RW_INTERLEAVED };
	static const unsigned int format[] = { SND_PCM_FORMAT_S32_LE };
	unsigned int period_bytes;
	unsigned int buffer_bytes;
	unsigned int periods;
	int result;

	period_bytes = pcm->period_size * pcm->channels * 4;
	buffer_bytes = pcm->buffer_size * pcm->channels * 4;
	periods = pcm->buffer_size / pcm->period_size;
	result = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_ACCESS,
						       1, access);
	if (result < 0)
		return result;
	result = snd_pcm_ioplug_set_param_list(io, SND_PCM_IOPLUG_HW_FORMAT,
						       1, format);
	if (result < 0)
		return result;
	result = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_CHANNELS,
							pcm->channels, pcm->channels);
	if (result < 0)
		return result;
	result = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_RATE,
							pcm->rate, pcm->rate);
	if (result < 0)
		return result;
	result = snd_pcm_ioplug_set_param_minmax(io, SND_PCM_IOPLUG_HW_PERIOD_BYTES,
							period_bytes, period_bytes);
	if (result < 0)
		return result;
	result = snd_pcm_ioplug_set_param_minmax(
		io, SND_PCM_IOPLUG_HW_BUFFER_BYTES,
		pcm->shared ? pcm->minimum_buffer_size * pcm->channels * 4 : buffer_bytes,
		buffer_bytes);
	if (result < 0)
		return result;
	return snd_pcm_ioplug_set_param_minmax(
		io, SND_PCM_IOPLUG_HW_PERIODS,
		pcm->shared ? pcm->minimum_buffer_size / pcm->period_size : periods,
		periods);
}

static const snd_pcm_ioplug_callback_t sidealsa_callback = {
	.start = sidealsa_start,
	.stop = sidealsa_stop,
	.pointer = sidealsa_pointer,
	.transfer = sidealsa_transfer,
	.close = sidealsa_close,
	.hw_params = sidealsa_hw_params,
	.sw_params = sidealsa_sw_params,
	.prepare = sidealsa_prepare,
	.drain = sidealsa_drain,
	.poll_descriptors_count = sidealsa_poll_descriptors_count,
	.poll_descriptors = sidealsa_poll_descriptors,
	.poll_revents = sidealsa_poll_revents,
};

static int sidealsa_parse_config(snd_config_t *conf, const char **socket,
					 const char **mode, const char **port)
{
	snd_config_iterator_t iterator, next;

	snd_config_for_each(iterator, next, conf) {
		snd_config_t *node = snd_config_iterator_entry(iterator);
		const char *id;
		const char *value;

		if (snd_config_get_id(node, &id) < 0)
			continue;
		if (!strcmp(id, "type") || !strcmp(id, "hint") || !strcmp(id, "comment"))
			continue;
		if (snd_config_get_string(node, &value) < 0)
			return -EINVAL;
		if (!strcmp(id, "socket"))
			*socket = value;
		else if (!strcmp(id, "mode"))
			*mode = value;
		else if (!strcmp(id, "port"))
			*port = value;
		else
			return -EINVAL;
	}
	return 0;
}

int sidealsa_plugin_open(snd_pcm_t **pcmp, const char *name,
				 snd_config_t *root, snd_config_t *conf,
				 snd_pcm_stream_t stream, int mode)
{
	const char *socket = "/tmp/sidealsad.sock";
	const char *mode_name = "pro";
	const char *port = NULL;
	sidealsa_pcm_t *pcm;
	unsigned int period_size;
	unsigned int minimum_buffer_size;
	unsigned int buffer_size;
	int sidealsa_mode;
	int direction;
	int result;

	(void)root;
	result = sidealsa_parse_config(conf, &socket, &mode_name, &port);
	if (result < 0)
		return result;
	if (!strcmp(mode_name, "pro")) {
		sidealsa_mode = 0;
	} else if (!strcmp(mode_name, "shared")) {
		if (!port)
			return -EINVAL;
		sidealsa_mode = 1;
	} else {
		return -EINVAL;
	}
	if (stream != SND_PCM_STREAM_PLAYBACK && stream != SND_PCM_STREAM_CAPTURE)
		return -EINVAL;
	direction = stream == SND_PCM_STREAM_PLAYBACK ? 0 : 1;

	pcm = calloc(1, sizeof(*pcm));
	if (!pcm)
		return -ENOMEM;
	pcm->poll_fd = -1;
	pcm->control_fd = -1;
	result = sidealsa_stream_open(socket, sidealsa_mode, port, direction,
					     !!(mode & SND_PCM_NONBLOCK), &pcm->stream,
					     &pcm->rate, &pcm->channels,
					     &period_size, &minimum_buffer_size, &buffer_size,
					     &pcm->poll_fd, &pcm->control_fd);
	if (result < 0)
		goto error;
	pcm->period_size = period_size;
	pcm->minimum_buffer_size = minimum_buffer_size;
	pcm->buffer_size = buffer_size;
	pcm->shared = sidealsa_mode == 1;

	pcm->io.version = SND_PCM_IOPLUG_VERSION;
	pcm->io.name = "SideALSA PCM";
	pcm->io.flags = SND_PCM_IOPLUG_FLAG_MONOTONIC |
			SND_PCM_IOPLUG_FLAG_BOUNDARY_WA;
	pcm->io.poll_fd = pcm->poll_fd;
	pcm->io.poll_events = POLLIN;
	pcm->io.mmap_rw = 0;
	pcm->io.callback = &sidealsa_callback;
	pcm->io.private_data = pcm;

	result = snd_pcm_ioplug_create(&pcm->io, name, stream, mode);
	if (result < 0)
		goto error;
	pcm->io.nonblock = !!(mode & SND_PCM_NONBLOCK);
	result = sidealsa_set_constraints(pcm);
	if (result < 0) {
		snd_pcm_ioplug_delete(&pcm->io);
		return result;
	}
	*pcmp = pcm->io.pcm;
	return 0;

error:
	if (pcm->stream)
		sidealsa_stream_close(pcm->stream);
	if (pcm->poll_fd >= 0)
		close(pcm->poll_fd);
	if (pcm->control_fd >= 0)
		close(pcm->control_fd);
	free(pcm);
	return result;
}
