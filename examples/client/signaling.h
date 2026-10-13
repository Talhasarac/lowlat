// The signaling service, from the connecting side: one socket per attempt,
// the app thread's.

#pragma once

#include <stdint.h>

#include "lowlat.h"

namespace signaling {

enum class Event {
	nothing,
	// `theirs` holds the host's credentials.
	answer,
	// `candidate` holds one of the host's, or its readiness marker.
	candidate,
	// The socket, the service or the host ended the attempt.
	closed,
};

bool connect(const char *server, const char *session, const char *peer, const char *attempt);
// One socket per attempt, closed once the path is up or the attempt is given
// up; `cancel` withdraws the offer first, for an attempt that never came up.
// Closing a closed socket does nothing, and nothing is sent on one.
void close(bool cancel);
bool offer(const lowlat_credentials *ours);
bool send_candidate(const char *address, uint16_t port, bool lan, bool from_stun, bool sync);
Event poll(uint32_t timeout_ms, lowlat_credentials *theirs, lowlat_candidate *candidate);

}
