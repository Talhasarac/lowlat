// What the window shows and where: the rectangle the picture is drawn into,
// told to the library, which maps positions into the picture, and the host's
// pointer over it. The app thread's, but stretched(), any thread.
//
// The host's pointer is drawn by the toolkit from the picture the library
// decodes, resampled with its hotspot by the ratio the picture is drawn at,
// so a pointer from a host at twice the scale shows at the size it has in
// the picture and shrinks with a letterboxed window; a pointer the host
// withholds for touch is hidden here too.

#pragma once

#include "lowlat.h"

namespace view {

// The window's size, read again on the toolkit's notice of a resize or a
// move rather than asked of the display on every pass, a round trip each.
void window_changed();
// Every pass: the rectangle, told to the library when it changes.
void place();
// The host's pointer: its picture kept if one came, then applied.
void cursor(const lowlat_cursor_event *c);
// The chord: stretched to the window, or at its own size when it fits.
void toggle_stretch();
bool stretched();
// The session left: the rectangle is told again with the next session's
// first picture, whatever its size.
void forget_session();

}
