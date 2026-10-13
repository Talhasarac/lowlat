// The pads. See pads.h.

#include "pads.h"

#include <stdio.h>
#include <string.h>

#if defined(__linux__)
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <unistd.h>
#endif

#include "app.h"
#include "config.h"
#include "session.h"

namespace pads {

// The Sony pads read raw from their own nodes, while the knob says so.

#define RAW_PADS 4

struct raw_pad {
	int fd;                    // -1 when the slot is empty
	unsigned node;             // the hidraw number
	uint32_t id;               // as named to the library
	uint32_t type;             // one of lowlat_pad_type
	bool wireless;             // the wireless framing on the node
	uint8_t seq;               // a wireless DualSense's output sequence
};

static raw_pad _raw[RAW_PADS];
// The second's counts: input reports sent, and the host's writes put on the
// nodes.
static uint32_t _raw_reports;
static uint32_t _raw_outputs;

static void raw_init()
{
	for (size_t i = 0; i < RAW_PADS; i++)
		_raw[i].fd = -1;
}

#if defined(__linux__)

#define VENDOR_SONY 0x054C
#define BUS_USB 3
#define BUS_BLUETOOTH 5
// The pads are named to the library apart from the toolkit's controller
// numbers, so a rumble message can be told to the right side, and **below
// 256**: an established host keys its pads on the low eight bits of the
// identifier for the state, button, axis and unplug messages and on the
// whole identifier for the report message, so a report under a wider
// identifier never reaches the pad the states made (docs/01-protocol.md
// 11.1), and its rumble comes back under the eight-bit one. The toolkit
// numbers its controllers by descriptor, so this starts well above where
// those sit.
#define ID_BASE 200
#define SCAN_EVERY_MS 1000.0

// When the nodes were last looked for.
static double _scanned_ms;

// The raw node's feature-report reads and writes.
#define HIDIOCGFEATURE(len) _IOC(_IOC_WRITE | _IOC_READ, 'H', 0x07, len)
#define HIDIOCSFEATURE(len) _IOC(_IOC_WRITE | _IOC_READ, 'H', 0x06, len)

// The two feature reports the host's driver asks for, per product and
// transport: calibration and firmware, identifier and length.
struct feature {
	uint8_t id;
	uint32_t len;
};

static void features_of(uint32_t type, bool wireless, struct feature out[2])
{
	if (type == LOWLAT_PAD_TYPE_DS4) {
		out[0] = feature{(uint8_t) (wireless ? 0x05 : 0x02), wireless ? 41u : 37u};
		out[1] = feature{0xA3, 49};
	} else {
		out[0] = feature{0x05, 41};
		out[1] = feature{0x20, 64};
	}
}

static uint32_t type_of(unsigned product)
{
	switch (product) {
		case 0x05C4:
		case 0x09CC: return LOWLAT_PAD_TYPE_DS4;
		case 0x0CE6: return LOWLAT_PAD_TYPE_DS5;
		default: return 0;
	}
}

// The node's identity from its device's uevent: bus, vendor, product, and
// whether it is one of a host's own virtual pads. A demo run on the host's
// machine would otherwise read the pad the host made from its reports and
// send it back, which is an echo.
static bool identity(unsigned node, unsigned *bus, unsigned *vendor, unsigned *product,
	bool *virtual_pad)
{
	char path[96];
	snprintf(path, sizeof path, "/sys/class/hidraw/hidraw%u/device/uevent", node);
	FILE *f = fopen(path, "r");
	if (f == NULL)
		return false;
	bool found = false;
	*virtual_pad = false;
	char line[160];
	while (fgets(line, sizeof line, f) != NULL) {
		if (sscanf(line, "HID_ID=%x:%x:%x", bus, vendor, product) == 3)
			found = true;
		if (strncmp(line, "HID_PHYS=lowlat/", 16) == 0)
			*virtual_pad = true;
	}
	fclose(f);
	return found;
}

static raw_pad *by_id(uint32_t id)
{
	for (size_t i = 0; i < RAW_PADS; i++)
		if (_raw[i].fd >= 0 && _raw[i].id == id)
			return &_raw[i];
	return NULL;
}

static void drop(raw_pad *p)
{
	printf("demo: pad %u on hidraw%u gone\n", p->id, p->node);
	::close(p->fd);
	p->fd = -1;
	lowlat_client *client = session::client();
	if (client != NULL)
		lowlat_client_send_pad_unplug(client, p->id);
}

// The feature reports, read from the pad and sent ahead of its first input
// report; one the pad does not answer is left to the host's default.
static void send_features(raw_pad *p)
{
	struct feature features[2];
	features_of(p->type, p->wireless, features);
	for (size_t i = 0; i < 2; i++) {
		uint8_t buf[64];
		memset(buf, 0, sizeof buf);
		buf[0] = features[i].id;
		int got = ioctl(p->fd, HIDIOCGFEATURE(features[i].len), buf);
		if (got <= 0) {
			printf("demo: pad %u feature 0x%02x not answered (%s)\n", p->id, features[i].id,
				strerror(errno));
			continue;
		}
		lowlat_status s = lowlat_client_send_pad_report(session::client(), p->id, p->type,
			LOWLAT_PAD_REPORT_FEATURE, buf, (uint32_t) got);
		if (config::get().trace_pads || s != LOWLAT_OK)
			printf("demo: pad %u feature 0x%02x %d bytes -> %s\n", p->id, features[i].id, got,
				lowlat_status_string(s));
	}
}

// Look for pads that appeared, every so often, once the session is up: a
// report before that has no session to travel on, and the feature reports go
// first.
static void raw_scan(double now_ms, bool established)
{
	if (!established || (_scanned_ms != 0.0 && now_ms - _scanned_ms < SCAN_EVERY_MS))
		return;
	_scanned_ms = now_ms;

	DIR *dir = opendir("/sys/class/hidraw");
	if (dir == NULL)
		return;
	struct dirent *entry;
	while ((entry = readdir(dir)) != NULL) {
		unsigned node;
		if (sscanf(entry->d_name, "hidraw%u", &node) != 1)
			continue;
		bool open_already = false;
		for (size_t i = 0; i < RAW_PADS; i++)
			if (_raw[i].fd >= 0 && _raw[i].node == node)
				open_already = true;
		if (open_already)
			continue;
		unsigned bus, vendor, product;
		bool virtual_pad;
		if (!identity(node, &bus, &vendor, &product, &virtual_pad) || vendor != VENDOR_SONY)
			continue;
		if (virtual_pad)
			continue;
		if ((bus != BUS_USB && bus != BUS_BLUETOOTH) || type_of(product) == 0)
			continue;
		raw_pad *slot = NULL;
		for (size_t i = 0; i < RAW_PADS && slot == NULL; i++)
			if (_raw[i].fd < 0)
				slot = &_raw[i];
		if (slot == NULL)
			break;
		char path[32];
		snprintf(path, sizeof path, "/dev/hidraw%u", node);
		int fd = open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
		if (fd < 0) {
			// Once, not every second: the seat's access rule is what
			// grants it, and a node without it stays without it.
			static bool said;
			if (!said)
				printf("demo: %s not readable (%s); the seat's access rule grants it\n", path,
					strerror(errno));
			said = true;
			continue;
		}
		*slot = raw_pad{
			.fd = fd,
			.node = node,
			.id = ID_BASE + node,
			.type = type_of(product),
			.wireless = bus == BUS_BLUETOOTH,
			.seq = 0,
		};
		printf("demo: pad %u is %s %04x on hidraw%u over %s, read raw\n", slot->id,
			slot->type == LOWLAT_PAD_TYPE_DS4 ? "DualShock 4" : "DualSense", product, node,
			slot->wireless ? "Bluetooth" : "USB");
		send_features(slot);
	}
	closedir(dir);
}

// Every report the nodes have queued, to the library.
static void raw_pump()
{
	for (size_t i = 0; i < RAW_PADS; i++) {
		raw_pad *p = &_raw[i];
		if (p->fd < 0)
			continue;
		// Everything queued, not one report a pass: a wired DualSense
		// reports several hundred times a second.
		for (;;) {
			uint8_t buf[LOWLAT_PAD_REPORT_MAX];
			ssize_t n = read(p->fd, buf, sizeof buf);
			if (n < 0) {
				if (errno == EAGAIN || errno == EINTR)
					break;
				drop(p);
				break;
			}
			if (n == 0) {
				drop(p);
				break;
			}
			lowlat_status s = lowlat_client_send_pad_report(session::client(), p->id, p->type,
				LOWLAT_PAD_REPORT_INPUT, buf, (uint32_t) n);
			if (s == LOWLAT_OK)
				_raw_reports++;
			else if (config::get().trace_pads)
				printf("demo: pad %u report %zd bytes id 0x%02x -> %s\n", p->id, n, buf[0],
					lowlat_status_string(s));
		}
	}
}

// Whether a pad the toolkit reports is one this side reads raw instead: the
// vendor's, from the start, so the host never gets a state for it under the
// toolkit's name before its node is open here. A node this side cannot open
// is said once, and that pad is then dead rather than sent both ways.
static bool raw_owns_vendor(uint16_t vid)
{
	return vid == VENDOR_SONY;
}

// The host's write, back to the pad it names. False when the pad is not one
// read here.
static bool raw_write(const lowlat_pad_report_event *e)
{
	raw_pad *p = by_id(e->pad);
	if (p == NULL)
		return false;
	int done;
	if (e->kind == LOWLAT_PAD_REPORT_FEATURE) {
		uint8_t buf[LOWLAT_PAD_REPORT_MAX];
		memcpy(buf, e->report, e->len);
		done = ioctl(p->fd, HIDIOCSFEATURE(e->len), buf);
	} else {
		done = (int) write(p->fd, e->report, e->len);
	}
	if (done < 0)
		printf("demo: pad %u write of %u bytes failed (%s)\n", p->id, (unsigned) e->len,
			strerror(errno));
	else
		_raw_outputs++;
	if (config::get().trace_pads) {
		// The head of the report, where the flags and the motors are.
		printf("demo: pad %u <- %s %u bytes:", p->id,
			e->kind == LOWLAT_PAD_REPORT_FEATURE ? "feature" : "output", (unsigned) e->len);
		for (uint32_t i = 0; i < e->len && i < 12; i++)
			printf(" %02x", e->report[i]);
		printf("\n");
	}
	return true;
}

// The wireless checksum: the direction's seed, then everything before the
// last four bytes, little endian at the end.
static void seal(uint8_t *report, size_t len)
{
	uint8_t seed = 0xA2;
	uint32_t crc = MTY_CRC32(0, &seed, 1);
	crc = MTY_CRC32(crc, report, len - 4);
	memcpy(report + len - 4, &crc, 4);
}

// The host's rumble message for a pad read here: a motor-only report, the
// lights untouched. False when the pad is not one read here.
static bool raw_rumble(uint32_t pad, uint8_t low, uint8_t high)
{
	raw_pad *p = by_id(pad);
	if (p == NULL)
		return false;
	// The motors alone: the flags name them and nothing else, so a lightbar
	// the host set stays as it is.
	uint8_t report[78];
	memset(report, 0, sizeof report);
	size_t len;
	if (p->type == LOWLAT_PAD_TYPE_DS4) {
		size_t at = p->wireless ? 3 : 1;
		report[0] = p->wireless ? 0x11 : 0x05;
		if (p->wireless)
			report[1] = 0xC0;
		report[at] = 0x01;         // motors valid
		report[at + 3] = high;     // right, the small one
		report[at + 4] = low;      // left, the large one
		len = p->wireless ? 78 : 32;
	} else {
		size_t at = p->wireless ? 3 : 1;
		report[0] = p->wireless ? 0x31 : 0x02;
		if (p->wireless) {
			// The sequence is the library's for its own writes; this one
			// interleaves with it, which the pad tolerates.
			report[1] = (uint8_t) (p->seq << 4);
			report[2] = 0x10;
			p->seq = (uint8_t) ((p->seq + 1) & 0x0F);
		}
		report[at] = 0x03;         // both motors, the compatible vibration
		report[at + 2] = high;     // right
		report[at + 3] = low;      // left
		len = p->wireless ? 78 : 48;
	}
	if (p->wireless)
		seal(report, len);
	if (write(p->fd, report, len) < 0)
		printf("demo: pad %u rumble write failed (%s)\n", p->id, strerror(errno));
	else
		_raw_outputs++;
	return true;
}

static void raw_close()
{
	for (size_t i = 0; i < RAW_PADS; i++)
		if (_raw[i].fd >= 0)
			drop(&_raw[i]);
}

#else

// No pad node to read here: the toolkit reads the Sony pads on this platform
// (docs/impl-plan-windows.md, W1.9), and the demo refuses LOWLAT_PAD_RAW, so
// every call the loop makes finds no pad.

static void raw_scan(double now_ms, bool established)
{
	(void) now_ms;
	(void) established;
}

static void raw_pump()
{
}

static bool raw_owns_vendor(uint16_t vid)
{
	(void) vid;
	return false;
}

static bool raw_write(const lowlat_pad_report_event *e)
{
	(void) e;
	return false;
}

static bool raw_rumble(uint32_t pad, uint8_t low, uint8_t high)
{
	(void) pad;
	(void) low;
	(void) high;
	return false;
}

static void raw_close()
{
}

#endif

// The toolkit's pads, sent once per pass, the latest state of each: the
// toolkit reports on every axis event, which is several hundred a second
// from a moving stick.
struct toolkit_pad {
	uint32_t id;
	lowlat_pad_state state;
	bool pending;
	// What sysfs says of the pad, looked up once: whether its kernel
	// driver names the face buttons by letter rather than by position,
	// and whether it is a pad the host on this same machine made (see
	// pad_facts).
	bool known;
	bool letter_named;
	bool hosts_own;
};

static toolkit_pad _pads[8];
static uint32_t _events;
static uint32_t _sent;
static uint32_t _rumbles;

// An axis scaled from the range the device reports to the wire's: sticks
// signed sixteen bit, triggers a byte.
static int32_t scaled(const MTY_Axis *a, int32_t lo, int32_t hi)
{
	int32_t span = (int32_t) a->max - (int32_t) a->min;
	if (span <= 0)
		return 0;
	int64_t v = ((int64_t) a->value - a->min) * (hi - lo);
	return (int32_t) (lo + (v + span / 2) / span);
}

// What sysfs says of a pad the toolkit reports, found by its identity.
//
// **Letter-named buttons.** The input layer has two names for each of the
// upper face buttons, and two conventions for using them: the PlayStation
// driver reports the button's position (north, west), the Xbox driver
// reports its letter -- and the letter codes are the position codes crossed
// (X is the north code, Y the west). The toolkit reads every pad by
// position, which puts an Xbox pad's X and Y on the wrong bits.
//
// **The host's own pads.** A demo run on the host's machine sees the pads
// that host made from this demo's own states -- the toolkit reports them
// like any other -- and sending them back makes more of them, up to the
// host's cap. Such a pad names its location as the host's. The toolkit
// gives no node for a controller, only its identity, so a real pad of the
// same identity on that machine is kept out with them.
//
// Both are Linux's: elsewhere a pad is taken as the toolkit reports it.
static void pad_facts(uint16_t vid, uint16_t pid, bool *letter_named, bool *hosts_own)
{
	*letter_named = false;
	*hosts_own = false;
#if !defined(__linux__)
	(void) vid;
	(void) pid;
#else
	DIR *dir = opendir("/sys/class/input");
	if (dir == NULL)
		return;
	struct dirent *entry;
	while ((entry = readdir(dir)) != NULL) {
		unsigned node;
		if (sscanf(entry->d_name, "event%u", &node) != 1)
			continue;
		char path[128];
		unsigned v = 0, p = 0;
		snprintf(path, sizeof path, "/sys/class/input/event%u/device/id/vendor", node);
		FILE *f = fopen(path, "r");
		if (f == NULL || fscanf(f, "%x", &v) != 1)
			v = 0;
		if (f != NULL)
			fclose(f);
		snprintf(path, sizeof path, "/sys/class/input/event%u/device/id/product", node);
		f = fopen(path, "r");
		if (f == NULL || fscanf(f, "%x", &p) != 1)
			p = 0;
		if (f != NULL)
			fclose(f);
		if (v != vid || p != pid)
			continue;
		snprintf(path, sizeof path, "/sys/class/input/event%u/device/phys", node);
		f = fopen(path, "r");
		if (f != NULL) {
			char phys[64] = {0};
			if (fgets(phys, sizeof phys, f) != NULL && strncmp(phys, "lowlat/", 7) == 0)
				*hosts_own = true;
			fclose(f);
		}
		snprintf(path, sizeof path, "/sys/class/input/event%u/device/device/driver", node);
		char driver[256];
		ssize_t n = readlink(path, driver, sizeof driver - 1);
		if (n <= 0)
			continue;
		driver[n] = '\0';
		const char *name = strrchr(driver, '/');
		if (strcmp(name != NULL ? name + 1 : driver, "xpad") == 0)
			*letter_named = true;
	}
	closedir(dir);
#endif
}

// A whole pad from the toolkit's report, kept as the latest state for the
// pad and sent on the next pass. Axes are found by their usage rather
// than their slot, because the toolkit numbers slots in the order the device
// lists its axes; a pad's sticks are X, Y, Z and Rz and its triggers Rx and
// Ry on that page. The toolkit hands every stick over as a signed sixteen-bit
// value with a stick pushed away from the player positive, which is the
// wire's own convention, so the values pass through as they are. (Until
// 2026-09-19 the vertical axes were negated here, on a misread trace, and a
// game on an established host looked the wrong way up.)
void controller(const MTY_ControllerEvent *c)
{
	const config::Settings &knobs = config::get();
	if (knobs.raw_pads_only || (knobs.raw_pads && raw_owns_vendor(c->vid)))
		return;
	_events++;
	size_t slot = sizeof _pads / sizeof _pads[0];
	for (size_t i = 0; i < sizeof _pads / sizeof _pads[0]; i++) {
		if (_pads[i].id == c->id)
			slot = i;
		else if (slot == sizeof _pads / sizeof _pads[0] && _pads[i].id == 0)
			slot = i;
	}
	if (slot == sizeof _pads / sizeof _pads[0])
		return;
	if (_pads[slot].id != c->id || !_pads[slot].known) {
		_pads[slot].id = c->id;
		_pads[slot].known = true;
		pad_facts(c->vid, c->pid, &_pads[slot].letter_named, &_pads[slot].hosts_own);
		if (_pads[slot].hosts_own)
			printf("demo: pad %u (%04x:%04x) is the host's own on this machine; not sent\n",
				c->id, c->vid, c->pid);
		else if (_pads[slot].letter_named)
			printf("demo: pad %u (%04x:%04x) names its buttons by letter; X and Y read back\n",
				c->id, c->vid, c->pid);
	}
	if (_pads[slot].hosts_own)
		return;
	bool crossed = _pads[slot].letter_named;
	lowlat_pad_state fresh{.size = sizeof(lowlat_pad_state)};
	lowlat_pad_state *p = &fresh;
	const struct { MTY_CButton from; uint16_t to; } BITS[] = {
		{MTY_CBUTTON_A, LOWLAT_PAD_STATE_A}, {MTY_CBUTTON_B, LOWLAT_PAD_STATE_B},
		{MTY_CBUTTON_X, (uint16_t) (crossed ? LOWLAT_PAD_STATE_Y : LOWLAT_PAD_STATE_X)},
		{MTY_CBUTTON_Y, (uint16_t) (crossed ? LOWLAT_PAD_STATE_X : LOWLAT_PAD_STATE_Y)},
		{MTY_CBUTTON_BACK, LOWLAT_PAD_STATE_BACK}, {MTY_CBUTTON_START, LOWLAT_PAD_STATE_START},
		{MTY_CBUTTON_LEFT_THUMB, LOWLAT_PAD_STATE_LSTICK},
		{MTY_CBUTTON_RIGHT_THUMB, LOWLAT_PAD_STATE_RSTICK},
		{MTY_CBUTTON_LEFT_SHOULDER, LOWLAT_PAD_STATE_LSHOULDER},
		{MTY_CBUTTON_RIGHT_SHOULDER, LOWLAT_PAD_STATE_RSHOULDER},
		{MTY_CBUTTON_GUIDE, LOWLAT_PAD_STATE_GUIDE},
		{MTY_CBUTTON_TOUCHPAD, LOWLAT_PAD_STATE_TOUCHPAD},
		{MTY_CBUTTON_DPAD_UP, LOWLAT_PAD_STATE_DPAD_UP},
		{MTY_CBUTTON_DPAD_DOWN, LOWLAT_PAD_STATE_DPAD_DOWN},
		{MTY_CBUTTON_DPAD_LEFT, LOWLAT_PAD_STATE_DPAD_LEFT},
		{MTY_CBUTTON_DPAD_RIGHT, LOWLAT_PAD_STATE_DPAD_RIGHT},
	};
	for (size_t i = 0; i < sizeof BITS / sizeof BITS[0]; i++)
		if (c->buttons[BITS[i].from])
			p->buttons |= BITS[i].to;
	for (uint8_t i = 0; i < c->numAxes && i < MTY_CAXIS_MAX; i++) {
		const MTY_Axis *a = &c->axes[i];
		switch (a->usage) {
			case 0x30: p->lx = (int16_t) scaled(a, -32768, 32767); break;
			case 0x31: p->ly = (int16_t) scaled(a, -32768, 32767); break;
			case 0x32: p->rx = (int16_t) scaled(a, -32768, 32767); break;
			case 0x35: p->ry = (int16_t) scaled(a, -32768, 32767); break;
			case 0x33: p->lt = (uint8_t) scaled(a, 0, 255); break;
			case 0x34: p->rt = (uint8_t) scaled(a, 0, 255); break;
			default: break;
		}
	}
	// A trigger pulled far enough is a button on some pads and only an axis
	// on others; the wire carries the axis.
	if (c->buttons[MTY_CBUTTON_LEFT_TRIGGER] && p->lt == 0)
		p->lt = 255;
	if (c->buttons[MTY_CBUTTON_RIGHT_TRIGGER] && p->rt == 0)
		p->rt = 255;
	_pads[slot].id = c->id;
	_pads[slot].state = fresh;
	_pads[slot].pending = true;
	if (knobs.trace_pads) {
		printf("pad %u (%04x:%04x):", c->id, c->vid, c->pid);
		for (uint8_t i = 0; i < c->numAxes && i < MTY_CAXIS_MAX; i++)
			printf(" u%02x=%d[%d..%d]", c->axes[i].usage, c->axes[i].value, c->axes[i].min,
				c->axes[i].max);
		printf(" -> lx=%d ly=%d rx=%d ry=%d lt=%u rt=%u buttons=%04x\n", p->lx, p->ly, p->rx,
			p->ry, p->lt, p->rt, p->buttons);
	}
}

void disconnected(const MTY_ControllerEvent *c)
{
	const config::Settings &knobs = config::get();
	if (knobs.raw_pads_only || (knobs.raw_pads && raw_owns_vendor(c->vid)))
		return;
	bool sent_ever = true;
	for (size_t i = 0; i < sizeof _pads / sizeof _pads[0]; i++)
		if (_pads[i].id == c->id) {
			sent_ever = !_pads[i].hosts_own;
			_pads[i] = toolkit_pad{};
		}
	if (sent_ever)
		lowlat_client_send_pad_unplug(session::client(), c->id);
}

// The latest state of each pad that reported since the last pass.
static void flush()
{
	for (size_t i = 0; i < sizeof _pads / sizeof _pads[0]; i++) {
		if (!_pads[i].pending)
			continue;
		_pads[i].pending = false;
		if (lowlat_client_send_pad_state(session::client(), _pads[i].id, &_pads[i].state)
			== LOWLAT_OK)
			_sent++;
	}
}

void start()
{
	raw_init();
}

void poll(double now_ms, bool established)
{
	if (config::get().raw_pads) {
		raw_scan(now_ms, established);
		raw_pump();
	}
	flush();
}

void rumble(const lowlat_rumble_event *r)
{
	// Eight bits each on the wire; the toolkit takes sixteen, and a byte
	// broadcast into both halves maps the ends exactly. A pad read raw gets
	// a motor-only report of its own instead.
	_rumbles++;
	if (config::get().raw_pads && raw_rumble(r->pad, r->low, r->high))
		return;
	MTY_AppRumbleController(app::toolkit(), r->pad, (uint16_t) (r->low | (r->low << 8)),
		(uint16_t) (r->high | (r->high << 8)));
}

void written(const lowlat_pad_report_event *e)
{
	if (!raw_write(e))
		printf("demo: a write for pad %u, which is not read here\n", (unsigned) e->pad);
}

void close()
{
	raw_close();
}

Second take_second()
{
	Second s = {_sent, _events, _raw_reports, _raw_outputs};
	_sent = 0;
	_events = 0;
	_raw_reports = 0;
	_raw_outputs = 0;
	return s;
}

uint32_t rumbles()
{
	return _rumbles;
}

}
