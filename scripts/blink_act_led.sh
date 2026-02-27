#!/usr/bin/env bash
set -euo pipefail

# Best-effort ACT LED blink helper for Raspberry Pi.
# It silently exits if no compatible LED sysfs path is found.

LED_PATH=""
for candidate in /sys/class/leds/ACT /sys/class/leds/act /sys/class/leds/led0; do
  if [[ -e "${candidate}/brightness" ]]; then
    LED_PATH="$candidate"
    break
  fi
done

if [[ -z "$LED_PATH" ]]; then
  exit 0
fi

BRIGHTNESS_FILE="${LED_PATH}/brightness"
TRIGGER_FILE="${LED_PATH}/trigger"
ORIGINAL_TRIGGER=""

if [[ -r "$TRIGGER_FILE" ]]; then
  ORIGINAL_TRIGGER="$(sed -n 's/.*\[\([^]]*\)\].*/\1/p' "$TRIGGER_FILE" | head -n1 || true)"
fi

if [[ -w "$TRIGGER_FILE" ]]; then
  echo none >"$TRIGGER_FILE" || true
fi

for _ in 1 2 3; do
  echo 1 >"$BRIGHTNESS_FILE" || true
  sleep 0.12
  echo 0 >"$BRIGHTNESS_FILE" || true
  sleep 0.12
done

if [[ -n "$ORIGINAL_TRIGGER" && -w "$TRIGGER_FILE" ]]; then
  echo "$ORIGINAL_TRIGGER" >"$TRIGGER_FILE" || true
fi
