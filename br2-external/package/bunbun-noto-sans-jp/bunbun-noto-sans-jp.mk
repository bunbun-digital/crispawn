# SPDX-License-Identifier: Apache-2.0
BUNBUN_NOTO_SANS_JP_VERSION = 2.004
BUNBUN_NOTO_SANS_JP_REVISION = f8d157532fbfaeda587e826d4cd5b21a49186f7c
BUNBUN_NOTO_SANS_JP_SITE = https://raw.githubusercontent.com/notofonts/noto-cjk/$(BUNBUN_NOTO_SANS_JP_REVISION)/Sans/SubsetOTF/JP
BUNBUN_NOTO_SANS_JP_SOURCE = NotoSansJP-Regular.otf
BUNBUN_NOTO_SANS_JP_EXTRA_DOWNLOADS = https://raw.githubusercontent.com/notofonts/noto-cjk/$(BUNBUN_NOTO_SANS_JP_REVISION)/Sans/LICENSE
BUNBUN_NOTO_SANS_JP_LICENSE = OFL-1.1
BUNBUN_NOTO_SANS_JP_LICENSE_FILES = LICENSE

define BUNBUN_NOTO_SANS_JP_EXTRACT_CMDS
	$(INSTALL) -m 0644 $(BUNBUN_NOTO_SANS_JP_DL_DIR)/$(BUNBUN_NOTO_SANS_JP_SOURCE) $(@D)/NotoSansJP-Regular.otf
	$(INSTALL) -m 0644 $(BUNBUN_NOTO_SANS_JP_DL_DIR)/LICENSE $(@D)/LICENSE
endef

define BUNBUN_NOTO_SANS_JP_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0644 $(@D)/NotoSansJP-Regular.otf \
		$(TARGET_DIR)/usr/share/fonts/bunbun/noto-sans-jp.otf
	$(INSTALL) -D -m 0644 $(@D)/LICENSE \
		$(TARGET_DIR)/usr/share/licenses/noto-sans-jp/LICENSE
endef

$(eval $(generic-package))
