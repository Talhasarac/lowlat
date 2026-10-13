// Sound: the host's, played on the toolkit's device from a thread of its own,
// and with LOWLAT_MIC_TONE a tone sent as the microphone from another. The
// device's own buffer paces playback and absorbs the drift; a resync is the
// device flushing, seen here as its queue at zero after it had been fed, or
// past the ceiling before a queue.
//
// start() and stop() are the main thread's; take_second(), the app thread's.

#pragma once

#include <stdint.h>

namespace sound {

// The device and the listening thread, and the tone's when one is asked for.
void start();
// After the loop has stopped: the threads joined, then the device.
void stop();

// The second's figures: packets and frames played, the device's queue now,
// at its lowest and highest, the oldest packet's age, the resyncs so far, and
// the tone's calls the library refused because the host takes no
// microphone. Read and cleared, but the queue now and the resyncs.
struct Second {
	uint32_t packets;
	uint32_t frames;
	uint32_t q_ms;
	uint32_t q_min;
	uint32_t q_max;
	uint32_t age_max;
	uint32_t resyncs;
	uint32_t mic_refused;
};
Second take_second();

}
