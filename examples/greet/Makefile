CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra
ELF2SELF ?= ../../target/release/elf2self
ifeq (,$(wildcard $(ELF2SELF)))
ELF2SELF = ../../target/debug/elf2self
endif
all: libgreet.so.1 libgreet.so libself-audit.so app app.self
libgreet.so.1: libgreet.c
	$(CC) $(CFLAGS) -fPIC -shared -Wl,-soname,libgreet.so.1 -o $@ libgreet.c
	ln -sf libgreet.so.1 libgreet.so
libself-audit.so: libself-audit.c
	$(CC) $(CFLAGS) -fPIC -shared -o $@ libself-audit.c -lsqlite3
app: app.c libgreet.so.1
	$(CC) $(CFLAGS) -o app app.c -L. -lgreet -Wl,-rpath,'$$ORIGIN' -Wl,-rpath,.
app.self: app
	$(ELF2SELF) app -o $@
clean:
	rm -f libgreet.so.1 libgreet.so libself-audit.so app app.self libmul.so.1
