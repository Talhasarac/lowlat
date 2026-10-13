// The presenting thread: the toolkit's graphics context is created there and
// stays there, and its loop is paced by the display through vsync. The main
// thread is the toolkit's event loop and must not be: the pads are read one
// event per iteration of it, so a loop bound to the display's rate drains a
// stick slower than it moves.
//
// start() and stop() are the main thread's; picture() and forget_picture(),
// any thread's; take_second(), the app thread's.

#pragma once

#include <stdint.h>

namespace present {

void start();
// After the loop has stopped: the thread joined, its picture released.
void stop();

// The picture on the screen as the rest of the demo reads it, published
// beside it with its width last: zero before there is one. The kind is the
// one it came as, which a decoder that hands out no handles makes planes
// whatever was asked.
struct Picture {
	uint32_t width;
	uint32_t height;
	uint32_t rotation;
	uint32_t format;
	uint32_t kind;
	bool full_range;
};
Picture picture();
// The session left: no picture until the next one's first.
void forget_picture();

// The second's figures, read and cleared: presents and polls, new pictures,
// repeats (a poll with no new picture) and skips (pictures published and
// never shown, a newer one having arrived); the wait from a new picture in
// hand to its present returning -- the draw and the display's next refresh
// -- summed, counted and at its longest; how many refreshes each picture
// replaced stayed up for (one, two, three, four or more); and a new
// picture's time from its arrival, as the library stamps it, to its present
// returning and to the acquire that took it, which is the library's share.
struct Second {
	uint32_t presents;
	uint32_t polls;
	uint32_t pictures;
	uint32_t repeats;
	uint32_t skips;
	uint32_t swap_sum_us;
	uint32_t swap_count;
	uint32_t swap_max_us;
	uint32_t held[4];
	uint32_t lat_sum_us;
	uint32_t lat_count;
	uint32_t lat_max_us;
	uint32_t lib_sum_us;
	uint32_t lib_max_us;
};
Second take_second();

}
