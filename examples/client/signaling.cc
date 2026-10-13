// The signaling service, spoken by the demo itself: the library carries no
// signaling (docs/04-signaling.md, docs/06-api.md section 4). One socket,
// authenticated by its query string; every message is
// { version, action, payload }; it carries the credentials and the
// candidates and nothing else, and it stops mattering once the path is up.

#include "signaling.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "matoya.h"

namespace signaling {

// The attempt's socket, and who and what it is for.
static MTY_WebSocket *_ws;
static char _peer[64];
static char _attempt[LOWLAT_ATTEMPT_MAX];

// Every message's `versions` block: what a peer of this generation says.
static MTY_JSON *versions()
{
	MTY_JSON *v = MTY_JSONObjCreate();
	MTY_JSONObjSetInt(v, "p2p", 1);
	MTY_JSONObjSetInt(v, "bud", 1);
	MTY_JSONObjSetInt(v, "init", 1);
	MTY_JSONObjSetInt(v, "video", 1);
	MTY_JSONObjSetInt(v, "audio", 1);
	MTY_JSONObjSetInt(v, "control", 1);
	return v;
}

static bool send_message(const char *action, MTY_JSON *payload)
{
	// Closed once the path is up: what the library still raises after that
	// has nowhere to go and needs nowhere.
	if (_ws == NULL) {
		MTY_JSONDestroy(&payload);
		return false;
	}
	MTY_JSON *message = MTY_JSONObjCreate();
	MTY_JSONObjSetInt(message, "version", 1);
	MTY_JSONObjSetString(message, "action", action);
	MTY_JSONObjSetItem(message, "payload", payload);
	char *text = MTY_JSONSerialize(message);
	bool ok = text != NULL && MTY_WebSocketWrite(_ws, text);
	MTY_Free(text);
	MTY_JSONDestroy(&message);
	if (!ok)
		fprintf(stderr, "signaling: %s was not sent\n", action);
	return ok;
}

bool connect(const char *server, const char *session, const char *peer, const char *attempt)
{
	_ws = NULL;
	snprintf(_peer, sizeof _peer, "%s", peer);
	snprintf(_attempt, sizeof _attempt, "%s", attempt);

	// No slash before the query: the toolkit's own URL parser carries one
	// into the Host header, and the edge answers 400 to that.
	char url[1024];
	snprintf(url, sizeof url,
		"wss://%s?session_id=%s&role=client&version=1&build=lowlat-client&sdk_version=0",
		server, session);

	uint16_t status = 0;
	_ws = MTY_WebSocketConnect(url, NULL, NULL, 10000, &status);
	if (_ws == NULL) {
		fprintf(stderr, "signaling: the service refused the socket, upgrade status %u\n",
			(unsigned) status);
		return false;
	}
	return true;
}

void close(bool cancel)
{
	if (_ws != NULL) {
		if (cancel) {
			MTY_JSON *payload = MTY_JSONObjCreate();
			MTY_JSONObjSetString(payload, "to", _peer);
			MTY_JSONObjSetString(payload, "attempt_id", _attempt);
			send_message("offer_cancel", payload);
		}
		MTY_WebSocketDestroy(&_ws);
	}
}

bool offer(const lowlat_credentials *ours)
{
	MTY_JSON *creds = MTY_JSONObjCreate();
	MTY_JSONObjSetString(creds, "ice_ufrag", ours->ufrag);
	MTY_JSONObjSetString(creds, "ice_pwd", ours->pwd);
	MTY_JSONObjSetString(creds, "fingerprint", ours->fingerprint);
	// The media key is a capability signal: present, both ends can take the
	// current cipher; absent, the host answers for the legacy one.
	if (ours->aes256[0] != '\0')
		MTY_JSONObjSetString(creds, "aes256", ours->aes256);

	MTY_JSON *data = MTY_JSONObjCreate();
	MTY_JSONObjSetInt(data, "ver_data", 1);
	MTY_JSONObjSetItem(data, "creds", creds);
	// The pipe, named: 1 is the native transport. An established host reads
	// this as a required integer and refuses the whole offer without it.
	MTY_JSONObjSetInt(data, "mode", 1);
	MTY_JSONObjSetItem(data, "versions", versions());

	MTY_JSON *payload = MTY_JSONObjCreate();
	MTY_JSONObjSetString(payload, "to", _peer);
	MTY_JSONObjSetString(payload, "attempt_id", _attempt);
	MTY_JSONObjSetString(payload, "secret", "");
	MTY_JSONObjSetItem(payload, "data", data);
	return send_message("offer", payload);
}

bool send_candidate(const char *address, uint16_t port, bool lan, bool from_stun, bool sync)
{
	MTY_JSON *data = MTY_JSONObjCreate();
	MTY_JSONObjSetInt(data, "ver_data", 1);
	MTY_JSONObjSetItem(data, "versions", versions());
	MTY_JSONObjSetString(data, "ip", address);
	MTY_JSONObjSetInt(data, "port", port);
	MTY_JSONObjSetBool(data, "lan", lan);
	MTY_JSONObjSetBool(data, "from_stun", from_stun);
	MTY_JSONObjSetBool(data, "sync", sync);

	MTY_JSON *payload = MTY_JSONObjCreate();
	MTY_JSONObjSetString(payload, "to", _peer);
	MTY_JSONObjSetString(payload, "attempt_id", _attempt);
	MTY_JSONObjSetItem(payload, "data", data);
	return send_message("candex", payload);
}

Event poll(uint32_t timeout_ms, lowlat_credentials *theirs, lowlat_candidate *candidate)
{
	if (_ws == NULL)
		return Event::closed;

	char text[16 * 1024];
	MTY_Async r = MTY_WebSocketRead(_ws, timeout_ms, text, sizeof text);
	if (r == MTY_ASYNC_CONTINUE)
		return Event::nothing;
	if (r != MTY_ASYNC_OK) {
		fprintf(stderr, "signaling: the socket ended, close code %u\n",
			(unsigned) MTY_WebSocketGetCloseCode(_ws));
		return Event::closed;
	}

	MTY_JSON *message = MTY_JSONParse(text);
	if (message == NULL) {
		// The toolkit names where the parse stopped; the length says whether
		// that was the end of a message cut short.
		fprintf(stderr, "signaling: a message did not parse, bytes=%zu\n", strlen(text));
		return Event::nothing;
	}
	Event event = Event::nothing;
	const char *action = MTY_JSONObjGetStringPtr(message, "action");
	const MTY_JSON *payload = MTY_JSONObjGetItem(message, "payload");
	const char *attempt = payload ? MTY_JSONObjGetStringPtr(payload, "attempt_id") : NULL;
	bool ours = attempt != NULL && strcmp(attempt, _attempt) == 0;

	if (action != NULL && strcmp(action, "answer_relay") == 0 && ours) {
		bool approved = false;
		MTY_JSONObjGetBool(payload, "approved", &approved);
		if (!approved) {
			fprintf(stderr, "signaling: the host declined\n");
			event = Event::closed;
		} else {
			const MTY_JSON *data = MTY_JSONObjGetItem(payload, "data");
			const MTY_JSON *creds = data ? MTY_JSONObjGetItem(data, "creds") : NULL;
			if (creds != NULL) {
				*theirs = lowlat_credentials{.size = sizeof(lowlat_credentials)};
				MTY_JSONObjGetString(creds, "ice_ufrag", theirs->ufrag, sizeof theirs->ufrag);
				MTY_JSONObjGetString(creds, "ice_pwd", theirs->pwd, sizeof theirs->pwd);
				MTY_JSONObjGetString(creds, "fingerprint", theirs->fingerprint,
					sizeof theirs->fingerprint);
				MTY_JSONObjGetString(creds, "aes256", theirs->aes256, sizeof theirs->aes256);
				event = Event::answer;
			}
		}

	} else if (action != NULL && strcmp(action, "candex_relay") == 0 && ours) {
		const MTY_JSON *data = MTY_JSONObjGetItem(payload, "data");
		if (data != NULL) {
			*candidate = lowlat_candidate{.size = sizeof(lowlat_candidate)};
			MTY_JSONObjGetString(data, "ip", candidate->address, sizeof candidate->address);
			int32_t port = 0;
			MTY_JSONObjGetInt(data, "port", &port);
			candidate->port = (uint16_t) port;
			MTY_JSONObjGetBool(data, "lan", &candidate->lan);
			MTY_JSONObjGetBool(data, "from_stun", &candidate->reflexive);
			MTY_JSONObjGetBool(data, "sync", &candidate->sync);
			event = Event::candidate;
		}

	} else if (action != NULL && strcmp(action, "close") == 0) {
		const char *reason = payload ? MTY_JSONObjGetStringPtr(payload, "reason") : NULL;
		fprintf(stderr, "signaling: the service closed the attempt: %s\n",
			reason ? reason : "no reason");
		event = Event::closed;
	}

	MTY_JSONDestroy(&message);
	return event;
}

}
