# PiStick

A Raspberry Pi 4 that works as a pocket computer with **no screen of its own**: its applications show up as **native windows, one by one**, on the computer or phone it is connected to. The processing stays on the Pi; the device you use only displays the windows and sends keyboard, mouse and touch input back.

## What makes it different

- **One window per application, not a whole screen.** VNC, RDP, scrcpy and similar tools stream the entire screen. Here each Pi window becomes a window of your own system, with Alt+Tab, resizing, clipboard and keyboard layout support.
- **Own clients for Windows and Android.** Tools like waypipe require a Linux client running Wayland.
- **A compositor that doesn't composite.** The server (`psd`) is a Wayland compositor that never draws a screen: it only forwards each window's buffer and the region that changed.
- **Hybrid codec.** Lossless blocks for UI and text (sharp and cheap to encode), hardware H.264 only where there is motion (video, games). It always sends the latest frame, which keeps latency low even over Wi-Fi.
- **Audio and microphone** in both directions, with a lossless codec written from scratch.
- **Secure by default.** 6-digit pairing code, encrypted channel, and client keys protected by the OS (DPAPI on Windows, Keystore on Android). Plain-text connections are refused.
- **Native code all the way.** Rust at the core, few dependencies, Java only for the Android app shell.

## How to use

Right now the connection goes over the network (Wi-Fi or cable). Direct USB mode is not ready yet.

**1. On the Raspberry Pi** (64-bit Raspberry Pi OS, Pi 4):

```sh
git clone https://github.com/mathjonatha/PiStick.git
cd PiStick
sh tools/pi/provisionar.sh        # installs dependencies and registers the pistick service
cargo build --release -p ps-server
```

The service announces the Pi on the network (mDNS). Applications running on the Pi (for example `foot`, `mousepad`, `mpv`) appear in the client.

**2. On Windows:**

```sh
cargo build --release -p ps-client
target\release\psc.exe --list     # finds the Pi on the network
target\release\psc.exe            # connects (or: psc.exe pi-name, psc.exe 192.168.0.10:7700)
```

**3. On Android:** build the APK with `tools/android/montar-apk.ps1` (needs the Android SDK/NDK and the Rust `aarch64-linux-android` target), install it, tap "Procurar aparelhos na rede" (search for devices) and pick the Pi.

**Pairing:** on first connection the client shows a 6-digit code. On the Pi, run `pistick-pair` and confirm that the code matches.

Phone gestures: tap = click; long press = right click; one-finger drag = selection; two fingers = scroll; pinch = zoom.

## Where it works

| Side | Platforms |
|---|---|
| Server | Raspberry Pi 4 (Raspberry Pi OS / Debian 12 or 13, arm64) |
| Client | Windows 10/11 and Android 11+ |
| In development | Direct USB, Linux and macOS clients |

Applications can be Linux (Wayland) or Android, through Waydroid.

## Layout

```
crates/   protocol, block codec, audio codec, transport, cryptography,
          discovery, server (psd), desktop client (psc) and the Android core
android/  Android app shell
tools/    provisioning and installation on the Pi, test scripts and measurements
```

Note: code comments and scripts are written in Brazilian Portuguese.

## License

The code is open: you may use, study, modify and redistribute it, **except for commercial purposes**. It is licensed under the [PolyForm Noncommercial License 1.0.0](LICENSE), and the only requirement beyond that is to **credit this project** (PiStick, github.com/mathjonatha/PiStick) when you use it or derive from it. Commercial use is not allowed without the author's written permission.
