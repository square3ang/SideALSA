/* Hardware-free callback contract test. Never creates or opens an ALSA PCM. */
#define snd_pcm_ioplug_avail test_ioplug_avail
#include "plugin.c"
#undef snd_pcm_ioplug_avail
#include <assert.h>

/* Pointer arithmetic is tested in Rust; do not dereference a real ALSA PCM. */
snd_pcm_uframes_t test_ioplug_avail(const snd_pcm_ioplug_t *io,
	snd_pcm_uframes_t hw, snd_pcm_uframes_t app)
{
	(void)io; (void)hw; (void)app;
	return 0;
}

struct sidealsa_stream {
	uint64_t expected, current, boundary, buffer, position;
	ssize_t transfer_result;
	int sync_result, sync_calls;
};

int sidealsa_stream_capture_sync(sidealsa_stream_t *s, uint64_t expected,
	uint64_t current, uint64_t boundary, uint64_t buffer)
{
	s->expected = expected;
	s->current = current;
	s->boundary = boundary;
	s->buffer = buffer;
	s->sync_calls++;
	return s->sync_result;
}
ssize_t sidealsa_stream_transfer(sidealsa_stream_t *s,
	const snd_pcm_channel_area_t *areas, size_t offset, size_t frames)
{
	(void)areas; (void)offset; (void)frames;
	return s->transfer_result;
}
uint64_t sidealsa_stream_position(const sidealsa_stream_t *s) { return s->position; }
int sidealsa_stream_prepare(sidealsa_stream_t *s) { s->sync_result = 0; return 0; }
int sidealsa_stream_set_nonblock(sidealsa_stream_t *s, int n) { (void)s; (void)n; return 0; }
int sidealsa_stream_set_buffer_size(sidealsa_stream_t *s, size_t n) { (void)s; (void)n; return 0; }
int sidealsa_stream_pump_pro_playback(sidealsa_stream_t *s) { (void)s; return 0; }
int sidealsa_stream_start(sidealsa_stream_t *s) { (void)s; return 0; }
int sidealsa_stream_stop(sidealsa_stream_t *s) { (void)s; return 0; }
int sidealsa_stream_drain(sidealsa_stream_t *s) { (void)s; return 0; }
int sidealsa_stream_close(sidealsa_stream_t *s) { (void)s; return 0; }
int sidealsa_stream_is_buffered_capture(const sidealsa_stream_t *s) { (void)s; return 1; }
void sidealsa_stream_record_playback_xrun(sidealsa_stream_t *s) { (void)s; }
int sidealsa_stream_open(const char *socket, int mode, const char *port,
	int direction, int nonblock, sidealsa_stream_t **stream,
	unsigned int *rate, unsigned int *channels, unsigned int *period,
	unsigned int *minimum, unsigned int *buffer, int *poll_fd, int *control_fd)
{
	(void)socket; (void)mode; (void)port; (void)direction; (void)nonblock;
	(void)stream; (void)rate; (void)channels; (void)period; (void)minimum;
	(void)buffer; (void)poll_fd; (void)control_fd;
	abort(); /* Any accidental open fails the test. */
}

int main(void)
{
	sidealsa_stream_t stream = { .position = 128, .transfer_result = 16 };
	sidealsa_pcm_t pcm = { .stream = &stream, .shared = 1,
		.buffered_capture = 1, .boundary = 4096 };
	snd_pcm_ioplug_t *io = &pcm.io;
	io->private_data = &pcm;
	io->stream = SND_PCM_STREAM_CAPTURE;
	io->buffer_size = 512;
	io->state = SND_PCM_STATE_PREPARED;
	assert(sidealsa_prepare(io) == 0);
	stream.sync_result = -EPIPE;
	assert(sidealsa_pointer(io) == 128);
	assert(stream.sync_calls == 0); /* status queries before start must not latch errors */
	stream.sync_result = 0;
	io->state = SND_PCM_STATE_RUNNING; /* libasound sets this after start */
	assert(sidealsa_transfer(io, NULL, 0, 64) == 16);
	assert(stream.expected == 0 && stream.current == 0);
	assert(io->appl_ptr == 0 && pcm.capture_expected_appl_ptr == 16);
	io->appl_ptr += 16; /* libasound's post-callback advance */
	assert(sidealsa_pointer(io) == 128);
	assert(stream.expected == 16 && stream.current == 16);
	io->appl_ptr += 64; /* snd_pcm_forward changes only ALSA's cursor */
	assert(sidealsa_pointer(io) == 128);
	assert(stream.expected == 16 && stream.current == 80);
	assert(stream.boundary == 4096 && stream.buffer == 512);
	assert(pcm.capture_expected_appl_ptr == 80);
	pcm.capture_expected_appl_ptr = io->appl_ptr = 4090;
	assert(sidealsa_transfer(io, NULL, 0, 64) == 16);
	assert(pcm.capture_expected_appl_ptr == 10);
	io->appl_ptr = 10;
	stream.sync_result = -EPIPE;
	assert(sidealsa_pointer(io) == -EPIPE);
	io->appl_ptr = 0; /* libasound resets BEFORE prepare */
	assert(sidealsa_prepare(io) == 0);
	io->state = SND_PCM_STATE_PREPARED;
	assert(pcm.capture_expected_appl_ptr == 0);
	assert(sidealsa_pointer(io) == 128);
	int calls = stream.sync_calls;
	pcm.shared = 0;
	io->state = SND_PCM_STATE_RUNNING;
	assert(sidealsa_transfer(io, NULL, 0, 64) == 16);
	assert(stream.sync_calls == calls + 1); /* directional PRO capture */
	assert(pcm.capture_expected_appl_ptr == 16);
	pcm.buffered_capture = 0; /* classic PRO capture */
	pcm.capture_expected_appl_ptr = 0;
	calls = stream.sync_calls;
	assert(sidealsa_transfer(io, NULL, 0, 64) == 16);
	assert(sidealsa_pointer(io) == 128);
	pcm.shared = 1;
	pcm.buffered_capture = 1; /* direction check is defensive too */
	io->stream = SND_PCM_STREAM_PLAYBACK;
	assert(sidealsa_transfer(io, NULL, 0, 64) == 16);
	assert(sidealsa_pointer(io) == 128);
	assert(stream.sync_calls == calls);
	assert(pcm.capture_expected_appl_ptr == 0);
	return 0;
}
