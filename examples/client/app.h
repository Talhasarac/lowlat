// The toolkit's app and its window: the main thread's loop, at the toolkit's
// own cadence, and the window's events, handed to the module each belongs
// to. The app thread's, but quit(), quitting() and the two handles, any
// thread.

#pragma once

#include "matoya.h"

namespace app {

// The app and its window; false, said on stderr, when either is not made.
bool create();
// The loop, until the window closes or anything asks to leave.
void run();
void destroy();

// Leave: the loop stops at its next pass, and every thread with it.
void quit();
bool quitting();

MTY_App *toolkit();
MTY_Window window();

}
