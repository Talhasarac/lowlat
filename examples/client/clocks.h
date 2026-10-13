// The two clocks the demo reads. Any thread.

#pragma once

#include <stddef.h>

namespace clocks {

// The clock the library stamps a picture's arrival on, so the two can be
// subtracted: the monotonic clock on Linux, the performance counter on
// Windows.
double now_ms();

// The local time of day to the millisecond, the clock a host's own log
// stamps its lines with, so the two can be read side by side.
void wall_time(char *out, size_t size);

// Now, kept as when the demo started, for the times the sound and latency
// lines carry; called once, before any thread that reads it starts.
double start();
double started_ms();

}
