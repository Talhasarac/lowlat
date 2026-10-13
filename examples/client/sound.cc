// Sound and the tone. See sound.h.

#include "sound.h"

#include <math.h>
#include <stdio.h>
#include <string.h>

#include <atomic>

#include "lowlat.h"
#include "matoya.h"

#include "app.h"
#include "clocks.h"
#include "config.h"
#include "session.h"

namespace sound {

static MTY_Audio *_audio;
static MTY_Thread *_listener;
static MTY_Thread *_mic;
// Whether the device is playing, and whether it only just started: the
// listening thread's alone.
static bool _playing;
static bool _just_started;

static std::atomic_uint _packets;
static std::atomic_uint _frames;
static std::atomic_uint _q_ms;
static std::atomic_uint _q_min{UINT32_MAX};
static std::atomic_uint _q_max;
static std::atomic_uint _age_max;
static std::atomic_uint _resyncs;
static std::atomic_uint _mic_refused;

// The tone thread: what a capture device would hand over, made up. Each pass
// sends as many samples as the clock says are due since the last, so the
// stream keeps time whatever the sleep does; the library cuts them into its
// own frames. Before a session, and while the host takes no microphone, the
// samples go nowhere.
static void *tone_loop(void *opaque) noexcept
{
	(void) opaque;
	lowlat_client *client = session::client();
	static int16_t pcm[4800];
	const double step = 6.283185307179586 * config::get().mic_hz / 48000.0;
	double phase = 0.0;
	double began = clocks::now_ms();
	uint64_t sent = 0;
	while (!app::quitting()) {
		MTY_Sleep(5);
		uint64_t due = (uint64_t) ((clocks::now_ms() - began) * 48.0);
		if (due <= sent)
			continue;
		// A thread held up for longer than the buffer starts again from now.
		if (due - sent > 4800)
			sent = due - 4800;
		uint32_t n = (uint32_t) (due - sent);
		for (uint32_t i = 0; i < n; i++) {
			pcm[i] = (int16_t) (sin(phase) * 8000.0);
			phase += step;
			if (phase > 6.283185307179586)
				phase -= 6.283185307179586;
		}
		if (lowlat_client_send_microphone(client, pcm, n) == LOWLAT_ERR_MICROPHONE_OFF)
			_mic_refused.fetch_add(1);
		sent += n;
	}
	return NULL;
}

// The listening thread: one packet at a time from the library, straight
// onto the device. The wait is the library's, long, so the thread costs
// nothing between packets; without a device it still acquires, so the
// figures are there on a box with no sound.
static void *sound_loop(void *opaque) noexcept
{
	(void) opaque;
	lowlat_client *client = session::client();
	bool trace = config::get().trace_audio;
	static int16_t pcm[8000 * 2];
	while (!app::quitting()) {
		uint32_t count = 8000;
		lowlat_status s = lowlat_client_acquire_audio(client, 100, pcm, &count);
		// Before a session is up, and between one and the next, the library
		// has nothing to wait on and answers at once, so the wait is made here
		// rather than spun: a core for as long as a connect takes.
		if (s == LOWLAT_ERR_NOT_STARTED)
			MTY_Sleep(20);
		if (s != LOWLAT_OK)
			continue;
		lowlat_client_status st{.size = sizeof(lowlat_client_status)};
		lowlat_client_get_status(client, &st);
		uint32_t queued = 0;
		if (_audio != NULL) {
			queued = MTY_AudioGetQueued(_audio);
			// The device's queue reads zero once more right after playback
			// (re)starts, before the device has reported anything; that
			// read is not a resync, and neither is the zero that follows a
			// flush this already counted. So a resync is counted while the
			// device is playing: the queue past the ceiling (the device
			// flushes on this call), or at zero (it ran dry), and then the
			// device is priming again until the floor is reached.
			const char *reason = NULL;
			if (_playing && !_just_started && queued == 0)
				reason = "empty";
			else if (_playing && queued > 150)
				reason = "over";
			if (reason != NULL) {
				uint32_t n = _resyncs.fetch_add(1) + 1;
				printf("demo: sound resync t=%.1f n=%u queued_ms=%u age_ms=%u reason=%s\n",
					(clocks::now_ms() - clocks::started_ms()) / 1000.0, n, queued,
					st.audio_age_ms, reason);
				fflush(stdout);
				_playing = false;
			}
			_just_started = false;
			if (!_playing && queued + count / 48 >= 75) {
				_playing = true;
				_just_started = true;
			}
			MTY_AudioQueue(_audio, pcm, count);
		}
		_packets.fetch_add(1);
		_frames.fetch_add(count);
		_q_ms.store(queued);
		if (queued < _q_min.load())
			_q_min.store(queued);
		if (queued > _q_max.load())
			_q_max.store(queued);
		if (st.audio_age_ms > _age_max.load())
			_age_max.store(st.audio_age_ms);
		if (trace)
			printf("demo: snd t_ms=%.1f frames=%u age_ms=%u queued_ms=%u\n",
				clocks::now_ms() - clocks::started_ms(), count, st.audio_age_ms, queued);
	}
	return NULL;
}

void start()
{
	// The device: stereo sixteen-bit at 48 kHz, 75 ms queued before it
	// plays and a flush past 150, which is the window a desktop client
	// runs. Without one the demo runs silent and still counts.
	MTY_AudioFormat format;
	memset(&format, 0, sizeof format);
	format.sampleFormat = MTY_AUDIO_SAMPLE_FORMAT_INT16;
	format.sampleRate = 48000;
	format.channels = 2;
	format.channelMask = MTY_AUDIO_CHANNEL_CFG_STEREO;
	_audio = MTY_AudioCreate(format, 75, 150, NULL, true);
	if (_audio == NULL)
		fprintf(stderr, "demo: no sound device, running silent\n");
	_listener = MTY_ThreadCreate(sound_loop, NULL);
	double mic_hz = config::get().mic_hz;
	if (mic_hz > 0.0) {
		printf("demo: sending a %.0f Hz tone as the microphone\n", mic_hz);
		_mic = MTY_ThreadCreate(tone_loop, NULL);
	}
}

void stop()
{
	MTY_ThreadDestroy(&_listener);
	if (_mic != NULL)
		MTY_ThreadDestroy(&_mic);
	if (_audio != NULL)
		MTY_AudioDestroy(&_audio);
}

Second take_second()
{
	Second s;
	s.packets = _packets.exchange(0);
	s.frames = _frames.exchange(0);
	s.q_min = _q_min.exchange(UINT32_MAX);
	s.q_max = _q_max.exchange(0);
	s.age_max = _age_max.exchange(0);
	s.q_ms = _q_ms.load();
	s.resyncs = _resyncs.load();
	s.mic_refused = _mic_refused.exchange(0);
	return s;
}

}
