CC ?= cc
PKG_CONFIG ?= pkg-config
override CPPFLAGS += $(shell $(PKG_CONFIG) --cflags lvgl)
override CFLAGS += -D_DEFAULT_SOURCE -std=c11 -Wall -Wextra -Werror -fPIC
LDLIBS += $(shell $(PKG_CONFIG) --libs lvgl)

libbunbun-ui.so.1: ui.c widgets.c bunbun/ui.h bunbun/widgets.h
	$(CC) $(CPPFLAGS) $(CFLAGS) -shared -Wl,-soname,libbunbun-ui.so.1 \
		-Wl,-z,defs -o $@ ui.c widgets.c $(LDFLAGS) $(LDLIBS)
