# The client demo on Windows: the C++ example on the application toolkit, built
# with Visual Studio's own tools from its x64 developer environment. The GNU
# build is GNUmakefile beside this one.
#
#   nmake           builds the toolkit, the library and the demo
#   nmake run       runs it; LOWLAT_PEER and LOWLAT_SESSION must be set
#
# The toolkit is built once, from its own tree with its own makefile, which
# needs its shader compiler (glslangValidator) on the path; the library is the
# SDK crate's release build; the demo links the two. The library's DLL is
# copied beside the demo, the first place the system's loader looks. The C
# and C++ runtimes are linked statically in every build, as the library links
# its own, so nothing asks the machine for a redistributable.
#
# LOWLAT_LIB names a directory holding a built lowlat.dll and its import
# library, lowlat.dll.lib, instead: no library is built, and the DLL is copied
# from there.

ROOT = ..\..
MATOYA = $(ROOT)\third_party\matoya
TARGET = $(ROOT)\target\release
MATOYA_LIB = $(MATOYA)\bin\windows\x64\matoya.lib

!IFNDEF LOWLAT_LIB
LIBDIR = $(TARGET)
LIBDEP = $(TARGET)\lowlat.dll
!ELSE
LIBDIR = $(LOWLAT_LIB)
LIBDEP =
!ENDIF

# C++20, its warnings errors in the demo's own code. The toolkit's header is C
# and stays outside them, since C++ calls what C allows there an extension; the
# compiler's advice against the standard library's string and file calls is
# declined, as the toolkit's own build declines it.
CXXFLAGS = /nologo /std:c++20 /EHsc /O2 /MT /W4 /WX /wd4100 \
	/D_CRT_SECURE_NO_WARNINGS /I$(ROOT)\include /external:I$(MATOYA)\src /external:W0

# What the toolkit calls into on this platform.
SYSTEM = user32.lib gdi32.lib shell32.lib ole32.lib oleaut32.lib uuid.lib \
	advapi32.lib ws2_32.lib bcrypt.lib crypt32.lib winmm.lib wininet.lib \
	winhttp.lib userenv.lib setupapi.lib cfgmgr32.lib iphlpapi.lib shlwapi.lib \
	imm32.lib hid.lib dxgi.lib d3d11.lib d3d12.lib d3dcompiler.lib dxguid.lib \
	dwmapi.lib dwrite.lib propsys.lib shcore.lib xinput.lib windowscodecs.lib

SOURCES = main.cc app.cc clocks.cc config.cc input.cc logs.cc pads.cc present.cc report.cc \
	session.cc signaling.cc sound.cc view.cc
HEADERS = app.h clocks.h config.h input.h keys.h logs.h pads.h present.h report.h session.h \
	signaling.h sound.h view.h

all: client.exe lowlat.dll

client.exe: $(SOURCES) $(HEADERS) $(MATOYA_LIB) $(LIBDEP)
	cl $(CXXFLAGS) /Fe$@ $(SOURCES) /link $(MATOYA_LIB) $(LIBDIR)\lowlat.dll.lib $(SYSTEM)

lowlat.dll: $(LIBDEP)
	copy /y $(LIBDIR)\lowlat.dll .

$(MATOYA_LIB):
	cmd /c "cd /d $(MATOYA) && nmake /nologo"

$(TARGET)\lowlat.dll: FORCE
	cargo build --manifest-path $(ROOT)\Cargo.toml -p lowlat-sdk --release

run: all
	client.exe

clean:
	-del /q client.exe $(SOURCES:.cc=.obj) lowlat.dll 2>nul

FORCE:
