// The client demo: a window that shows a host's desktop and drives it.
//
// C++20 on the application toolkit, with SDL for the pads and the sound. The
// library decodes and encodes input; this presents and reports what happened
// in its window. A file for each part, each holding its own state: the app
// and its window (app), the knobs (config), the session and its attempts
// (session), the signaling service (signaling), the presenting thread
// (present), what the window shows and where (view), keyboard and mouse
// (input), pads (pads), sound and the microphone's tone (sound), the line
// each second (report), and the clocks and lines (clocks, logs). The knobs
// are config.h's.
//
//   LOWLAT_PEER=<the host's peer id> LOWLAT_SESSION=<a session token> ./client
//
// Keyboard and mouse go to the host as the toolkit reports them, pads as SDL
// does; the rectangle the picture is drawn into is told to the library, which
// maps positions into the picture. Chords the demo keeps for itself, never sent,
// Ctrl+Shift and a letter: I grabs the keyboard so the desktop's own keys go
// to the host (and lets it go), W toggles fullscreen, D asks the host to
// stream its next output, F switches between the picture stretched to the
// window and shown at its own size, R lets go of a pointer the host has
// captured (and takes it again), C cycles the colour preferences, X moves
// the session to the next decoder this machine listed at start, H switches
// the pictures between planes and device handles at the next one, Q leaves
// the session cleanly and quits, as closing the window does.
// A bare Windows key is not sent while the keyboard is not grabbed, because
// the desktop here takes it and the host would be left with the modifier
// held; it reaches the host on chords, and whole once grabbed.

#include <stdio.h>

#include "lowlat.h"
#include "matoya.h"

#include "app.h"
#include "clocks.h"
#include "config.h"
#include "logs.h"
#include "pads.h"
#include "present.h"
#include "report.h"
#include "session.h"
#include "sound.h"

int main()
{
	// Whichever library the run path resolved to, first: every run's log
	// then names the object it ran against, and a packaged demo can be
	// asked what it carries without a peer.
	uint32_t abi = lowlat_abi_version();
	fprintf(stderr, "demo: library loaded, abi=%u.%u features=0x%x\n",
		abi >> 16, abi & 0xffff, (unsigned) lowlat_features());
	if ((lowlat_features() & LOWLAT_FEATURE_CLIENT) == 0) {
		fprintf(stderr, "demo: this library carries no client half\n");
		return 2;
	}
	int code = config::read();
	if (code != 0)
		return code;

	lowlat_set_log_callback(logs::library_line, NULL);
	MTY_SetLogFunc(logs::toolkit_line, NULL);
	lowlat_set_log_level(LOWLAT_LOG_INFO);

	pads::start();
	if (!session::create())
		return 1;
	char memory[64];
	report::memory_words(memory, sizeof memory);
	printf("demo: %s after creation\n", memory);
	if (config::get().nat)
		session::probe_nat();
	if (!session::begin())
		return 1;

	if (!app::create())
		return 1;
	double began = clocks::start();
	report::start(began);
	session::schedule_leave(began);
	// The toolkit's threads, which never come back empty: it aborts instead.
	present::start();
	sound::start();

	app::run();

	app::quit();
	present::stop();
	sound::stop();
	pads::close();
	session::end();
	// The window goes before the handle its events call into: destroying it
	// raises events of its own (on Windows the focus it loses, whose release
	// reaches the library), so the handle must still be there.
	app::destroy();
	session::destroy();
	return 0;
}
