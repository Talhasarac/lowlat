// Pads: every controller through SDL, sent as the sixteen-button pad every
// host takes, or a DualShock 4 or a DualSense as its own report
// (docs/10-client.md section 8). The app thread's.
//
// SDL's gamepads give the states, the latest of each pad that changed sent
// once a pass while the window is in front. A Sony pad that is to travel as
// its own report is read through SDL's HID layer instead and never opened as
// a gamepad, so nothing but the host writes it: its feature reports read and
// sent ahead of its first input report in each session, every input report
// sent as it came, and what the host's device is written put back on it. A
// rumble goes to the pad the host named.

#pragma once

#include <stdint.h>

#include "lowlat.h"

namespace pads {

// Before the session, on the main thread, which the loop runs on: SDL's
// gamepads. This is the demo's first call into SDL, which makes the main
// thread the one SDL is pumped on.
void start();
// Every pass of the loop: SDL's events taken, the raw pads looked for once the
// session is up and read, and the latest state of each gamepad that changed
// sent while the window is in front.
void poll(bool established);
// The host's rumble, and its writes for a pad read raw.
void rumble(const lowlat_rumble_event *r);
void written(const lowlat_pad_report_event *e);
// At the end: each raw pad closed and said unplugged, then SDL's gamepads.
void close();

// The second's counts, read and cleared: states sent, SDL's pad events, and
// the raw pads' input reports sent and the host's writes put on them.
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
