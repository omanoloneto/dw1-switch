ifeq ($(strip $(DEVKITPRO)),)
$(error DEVKITPRO is not set: export DEVKITPRO=/opt/devkitpro)
endif

include $(DEVKITPRO)/libnx/switch_rules

TARGET      := dw1
BUILD       := build
RUST_LIB    ?= ../target/aarch64-nintendo-switch-newlib/release/libdw1_switch.a

APP_TITLE   := Digimon World
APP_AUTHOR  := dw1-switch
APP_VERSION := 0.1.0
ICON        ?= icon.jpg

PKG_CONFIG  := $(DEVKITPRO)/portlibs/switch/bin/aarch64-none-elf-pkg-config

ARCH        := -march=armv8-a+crc+crypto -mtune=cortex-a57 -mtp=soft -fPIE
CFLAGS      := -g -O2 -Wall -ffunction-sections -fno-omit-frame-pointer -mno-omit-leaf-frame-pointer \
               $(ARCH) -D__SWITCH__ -I$(LIBNX)/include $(shell $(PKG_CONFIG) --cflags sdl2)
LDFLAGS     := -specs=$(LIBNX)/switch.specs -g $(ARCH) -Wl,-Map,$(BUILD)/$(TARGET).map -Wl,--wrap=pthread_create
LIBS        := $(shell $(PKG_CONFIG) --libs sdl2) -lnx -lm

OBJS        := $(patsubst source/%.c,$(BUILD)/%.o,$(wildcard source/*.c))

.PHONY: all clean

all: $(TARGET).nro

$(BUILD):
	@mkdir -p $@

$(BUILD)/%.o: source/%.c | $(BUILD)
	$(CC) $(CFLAGS) -c $< -o $@

$(BUILD)/$(TARGET).elf: $(OBJS) $(RUST_LIB)
	$(CC) $(LDFLAGS) $(OBJS) $(RUST_LIB) $(LIBS) -o $@

$(BUILD)/$(TARGET).nacp: | $(BUILD)
	nacptool --create "$(APP_TITLE)" "$(APP_AUTHOR)" "$(APP_VERSION)" $@

$(TARGET).nro: $(BUILD)/$(TARGET).elf $(BUILD)/$(TARGET).nacp
	elf2nro $< $@ --icon=$(ICON) --nacp=$(BUILD)/$(TARGET).nacp

clean:
	rm -rf $(BUILD) $(TARGET).nro
