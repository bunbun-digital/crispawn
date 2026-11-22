# SPDX-License-Identifier: Apache-2.0
BUNBUN_UI_RUNTIME_VERSION = 1.2
BUNBUN_UI_RUNTIME_SITE = $(BR2_EXTERNAL_BUNBUN_PATH)/package/bunbun-ui-runtime/src
BUNBUN_UI_RUNTIME_SITE_METHOD = local
BUNBUN_UI_RUNTIME_LICENSE = Apache-2.0
BUNBUN_UI_RUNTIME_LICENSE_FILES = LICENSE
BUNBUN_UI_RUNTIME_DEPENDENCIES = bunbun-lvgl bunbun-noto-sans-jp host-pkgconf
BUNBUN_UI_RUNTIME_INSTALL_STAGING = YES

define BUNBUN_UI_RUNTIME_BUILD_CMDS
	$(TARGET_MAKE_ENV) $(MAKE) $(TARGET_CONFIGURE_OPTS) -C $(@D)
endef

define BUNBUN_UI_RUNTIME_INSTALL_STAGING_CMDS
	$(INSTALL) -D -m 0755 $(@D)/libbunbun-ui.so.1 $(STAGING_DIR)/usr/lib/libbunbun-ui.so.1
	ln -sf libbunbun-ui.so.1 $(STAGING_DIR)/usr/lib/libbunbun-ui.so
	$(INSTALL) -D -m 0644 $(@D)/bunbun/ui.h $(STAGING_DIR)/usr/include/bunbun/ui.h
	$(INSTALL) -D -m 0644 $(@D)/bunbun/widgets.h $(STAGING_DIR)/usr/include/bunbun/widgets.h
	$(INSTALL) -D -m 0644 $(@D)/bunbun-ui.pc $(STAGING_DIR)/usr/lib/pkgconfig/bunbun-ui.pc
endef

define BUNBUN_UI_RUNTIME_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/libbunbun-ui.so.1 $(TARGET_DIR)/usr/lib/libbunbun-ui.so.1
	$(INSTALL) -D -m 0644 $(@D)/LICENSE $(TARGET_DIR)/usr/share/licenses/bunbun-ui-runtime/LICENSE
endef

$(eval $(generic-package))
