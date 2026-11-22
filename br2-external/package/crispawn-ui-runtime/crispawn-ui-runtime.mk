# SPDX-License-Identifier: Apache-2.0
CRISPAWN_UI_RUNTIME_VERSION = 1.2
CRISPAWN_UI_RUNTIME_SITE = $(BR2_EXTERNAL_CRISPAWN_PATH)/package/crispawn-ui-runtime/src
CRISPAWN_UI_RUNTIME_SITE_METHOD = local
CRISPAWN_UI_RUNTIME_LICENSE = Apache-2.0
CRISPAWN_UI_RUNTIME_LICENSE_FILES = LICENSE
CRISPAWN_UI_RUNTIME_DEPENDENCIES = crispawn-lvgl crispawn-noto-sans-jp host-pkgconf
CRISPAWN_UI_RUNTIME_INSTALL_STAGING = YES

define CRISPAWN_UI_RUNTIME_BUILD_CMDS
	$(TARGET_MAKE_ENV) $(MAKE) $(TARGET_CONFIGURE_OPTS) -C $(@D)
endef

define CRISPAWN_UI_RUNTIME_INSTALL_STAGING_CMDS
	$(INSTALL) -D -m 0755 $(@D)/libcrispawn-ui.so.1 $(STAGING_DIR)/usr/lib/libcrispawn-ui.so.1
	ln -sf libcrispawn-ui.so.1 $(STAGING_DIR)/usr/lib/libcrispawn-ui.so
	$(INSTALL) -D -m 0644 $(@D)/crispawn/ui.h $(STAGING_DIR)/usr/include/crispawn/ui.h
	$(INSTALL) -D -m 0644 $(@D)/crispawn/widgets.h $(STAGING_DIR)/usr/include/crispawn/widgets.h
	$(INSTALL) -D -m 0644 $(@D)/crispawn-ui.pc $(STAGING_DIR)/usr/lib/pkgconfig/crispawn-ui.pc
endef

define CRISPAWN_UI_RUNTIME_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/libcrispawn-ui.so.1 $(TARGET_DIR)/usr/lib/libcrispawn-ui.so.1
	$(INSTALL) -D -m 0644 $(@D)/LICENSE $(TARGET_DIR)/usr/share/licenses/crispawn-ui-runtime/LICENSE
endef

$(eval $(generic-package))
