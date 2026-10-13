// Once a second, a line to stdout with the figures, and the same in the
// window's title bar so a session can be read at a glance. The app thread's.
//
// The line carries the presentation cadence as numbers rather than a
// judgement: presents and pictures in the second, repeats (a present with no
// new picture) and skips (pictures published and never shown, because a
// newer one had arrived), the wait from a new picture in hand to its present
// returning (on average and at most: the draw and the display's next
// refresh), how many refreshes each picture stayed up for (one, two, three,
// four or more), a new picture's time from its arrival to its present
// returning and, of that, to the acquire that took it (on average and at
// most; `LOWLAT_LATENCY_TRACE` prints both per picture, with the motion sent
// so far), the decoder's figures, the reader's lag, and the process's
// resident set; then this side's figures for the video channel (fragments,
// late arrivals, negatives sent, the recent loss) beside the host's figures
// for this guest as its last guest list carried them (`h_*`), which are the
// two ends of one path.

#pragma once

#include <stddef.h>
#include <stdint.h>

namespace report {

// When the seconds start counting: the window up, before the threads.
void start(double now_ms);
// Every pass of the loop: a second past, its line and its title; true when
// one was.
bool tick(double now_ms);
// The seconds reported so far, and whether the session was up at the last.
uint64_t seconds();
bool established();
// The session left: its figures and the warning forgotten.
void forget_session();

// The process's memory as the platform charges it, for the lines: what is
// resident, and on Windows what is committed as well, which the system
// charges whether it is ever touched or not.
void memory_words(char *out, size_t size);

}
