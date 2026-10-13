// The two clocks the demo reads. See clocks.h.

#include "clocks.h"

#include <stdio.h>
#include <time.h>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

namespace clocks {

static double _started_ms;

double now_ms()
{
#if defined(_WIN32)
	LARGE_INTEGER count, frequency;
	QueryPerformanceCounter(&count);
	QueryPerformanceFrequency(&frequency);
	return (double) count.QuadPart * 1000.0 / (double) frequency.QuadPart;
#else
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double) ts.tv_sec * 1000.0 + (double) ts.tv_nsec / 1.0e6;
#endif
}

void wall_time(char *out, size_t size)
{
#if defined(_WIN32)
	SYSTEMTIME t;
	GetLocalTime(&t);
	snprintf(out, size, "%02u:%02u:%02u.%03u", (unsigned) t.wHour, (unsigned) t.wMinute,
		(unsigned) t.wSecond, (unsigned) t.wMilliseconds);
#else
	struct timespec ts;
	struct tm t;
	clock_gettime(CLOCK_REALTIME, &ts);
	localtime_r(&ts.tv_sec, &t);
	snprintf(out, size, "%02d:%02d:%02d.%03ld", t.tm_hour, t.tm_min, t.tm_sec,
		ts.tv_nsec / 1000000);
#endif
}

double start()
{
	_started_ms = now_ms();
	return _started_ms;
}

double started_ms()
{
	return _started_ms;
}

}
