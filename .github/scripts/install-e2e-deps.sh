#!/usr/bin/env bash
# The packages every e2e job needs: the native libraries the framework, the
# GTK backend and the water CLI link against, a software Vulkan/GL stack for
# the headless runner, the X session tooling the capture uses, and the CJK,
# Arabic and Hebrew fonts the text examples shape (without them every
# non-Latin run is tofu and the golden records the missing font, not the
# backend).
set -euo pipefail
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
    nasm libasound2-dev libva-dev libfontconfig1-dev libgbm-dev \
    libxcb1-dev libglib2.0-dev libpango1.0-dev libgdk-pixbuf-2.0-dev \
    libgtk-4-dev libadwaita-1-dev libwebkitgtk-6.0-dev \
    libpipewire-0.3-dev libudev-dev libegl-dev libegl1-mesa-dev \
    libgles2-mesa-dev libepoxy-dev libgraphene-1.0-dev libwayland-dev \
    libxkbcommon-dev mesa-vulkan-drivers libvulkan1 libgl1-mesa-dri \
    pkg-config \
    xvfb openbox dbus-x11 imagemagick xdotool x11-utils \
    fonts-noto-cjk fonts-noto-core fonts-noto-color-emoji
