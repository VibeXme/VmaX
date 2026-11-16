# Bundled internet helper

`cloudflared.exe` is the unmodified Cloudflare Windows amd64 2026.10.7
binary. Its Windows Authenticode signature was verified as valid, issued to
Cloudflare, Inc. The Apache 2.0 license is in `cloudflared-LICENSE`.
Source and releases: https://github.com/cloudflare/cloudflared

The Windows executable embeds the helper and extracts it to app-local data
on demand, verifying its bytes against the embedded copy before launch.
Android does not embed or launch it. VmaX owns the helper's process and binds
its signaling server only to a random loopback port. Closing VmaX stops it.

This is a third-party reachability service, not a proprietary NAT bypass.
Quick Tunnel URLs change after restart, have no uptime guarantee, and are
intended for testing. Production needs a provisioned public VmaX relay or
named tunnel. Do not hardcode a temporary URL as a shipping default.

`ViGEmBus.exe` is the official 1.22.0 installer from
https://github.com/nefarius/ViGEmBus/releases/tag/v1.22.0 . Its Authenticode
signature was verified as valid, signed by Nefarius Software Solutions e.U.
SHA-256: `89220A7865076B342892F98865F3499FB7C4CFD673159E89D352C360FD014C6A`.
It is embedded only for Windows and launched only when the user selects driver
installation; Windows controls elevation and the installer UI. ViGEmBus has
reached end of life. Its upstream license is in `ViGEmBus-LICENSE`.
