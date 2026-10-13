// Pads: the toolkit's controllers, sent as the sixteen-button pad every host
// takes, and the Sony pads read raw from their own nodes beside them
// (docs/10-client.md section 8), sent as their own report. The app thread's.
//
// The toolkit has no HID path on Linux, so a DualShock 4 or a DualSense that
// is to travel as its own report is read here: its raw node found by
// identity, polled in the demo's loop, its feature reports read once at open
// and sent ahead of the first input report, and what the host's device is
// written put back on the node. The toolkit's own events for these pads are
// dropped while this is on, so nothing is sent twice. A rumble goes to the
// pad the host named.

#pragma once

#include <stdint.h>

#include "lowlat.h"
#include "matoya.h"

namespace pads {

// Before the session: the raw pads' slots, empty.
void start();
// Every pass of the loop: the raw pads looked for once the session is up and
// read, and the latest state of each toolkit pad that reported, sent.
void poll(double now_ms, bool established);
void controller(const MTY_ControllerEvent *c);
void disconnected(const MTY_ControllerEvent *c);
// The host's rumble, and its writes for a pad read raw.
void rumble(const lowlat_rumble_event *r);
void written(const lowlat_pad_report_event *e);
// At the end: each raw pad's node closed, and the pad said unplugged.
void close();

// The second's counts, read and cleared: states sent, the toolkit's pad
// events, and the raw pads' input reports sent and the host's writes put on
// their nodes.
struct Second {
	uint32_t sent;
	uint32_t events;
	uint32_t raw_reports;
	uint32_t raw_outputs;
};
Second take_second();
// Every rumble the host sent, never cleared.
uint32_t rumbles();

}
