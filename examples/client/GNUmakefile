# The client demo: the C++ example on the application toolkit.
#
#   make            builds the toolkit, the library and the demo
#   make run        runs it; LOWLAT_PEER and LOWLAT_SESSION must be set
#
# The toolkit is built from its own tree with its own makefile; the library
# is the SDK crate's release build; the demo links the two. Nothing here
# knows a package manager.
#
# LOWLAT_LIB names a directory holding a built liblowlat.so instead: no
# library is built, the demo links the one there, and its run path is
# ../lib relative to itself, which is where a packaged demo finds it.

ROOT    := $(abspath ../..)
MATOYA  := $(ROOT)/third_party/matoya
ARCH    := $(shell uname -m)
TARGET  := $(ROOT)/target/release

ifeq ($(LOWLAT_LIB),)
LIBDIR  := $(TARGET)
LIBDEP  := $(TARGET)/liblowlat.so
RPATH   := $(TARGET)
else
LIBDIR  := $(LOWLAT_LIB)
LIBDEP  :=
RPATH   := '$$ORIGIN/../lib'
endif

# g++ counts the members a designated initializer leaves out, which C++ zeroes
# by definition: the library's structs are set up by their size alone.
CXXFLAGS := -std=c++20 -O2 -Wall -Wextra -Wno-unused-parameter -Wno-missing-field-initializers \
           -I$(ROOT)/include -I$(MATOYA)/src
# The one-off probes below are C.
CFLAGS  := -std=c11 -D_POSIX_C_SOURCE=200809L -O2 -Wall -Wextra -Wno-unused-parameter \
           -I$(ROOT)/include -I$(MATOYA)/src
LIBS    := $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a \
           -L$(LIBDIR) -llowlat -Wl,-rpath,$(RPATH) \
           -lc -ldl -lpthread -lm

SOURCES := main.cc signaling.cc pads.cc

all: client

# The C++ runtime is linked in, so a packaged demo needs no particular one on
# the machine it lands on.
client: $(SOURCES) signaling.h pads.h keys.h $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a $(LIBDEP)
	$(CXX) $(CXXFLAGS) -o $@ $(SOURCES) $(LIBS) -static-libstdc++

$(MATOYA)/bin/linux/$(ARCH)/libmatoya.a:
	TERM=xterm $(MAKE) -C $(MATOYA)

$(TARGET)/liblowlat.so: FORCE
	cd $(ROOT) && cargo build -p lowlat-sdk --release

run: client
	./client

# A one-off: does the toolkit's GL context import what the compute runtime
# exports? Needs no library build.
probe-import: probe-import.c $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a
	$(CC) $(CFLAGS) -I$(MATOYA)/deps -o $@ probe-import.c \
		$(MATOYA)/bin/linux/$(ARCH)/libmatoya.a -lc -ldl -lpthread -lm

# The toolkit's websocket reader against a loopback server that sends
# messages in fragments. Needs no library build and no network.
ws-check: ws-check.c $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a
	$(CC) $(CFLAGS) -o $@ ws-check.c $(MATOYA)/bin/linux/$(ARCH)/libmatoya.a \
		-lc -ldl -lpthread -lm

check: ws-check
	./ws-check

clean:
	rm -f client probe-import ws-check

FORCE:

.PHONY: all run clean FORCE
