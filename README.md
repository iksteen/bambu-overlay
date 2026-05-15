# bambu-overlay

Rust rewrite of the Bambu overlay prototype.

The release build is a single deployable binary. HTML, CSS, and browser
JavaScript are embedded at compile time with `include_str!`; no external static
files are needed next to the binary.

## Build

```sh
cargo build --release
```

On Linux, the video TLS transport uses `native-tls`, which links against
OpenSSL. Install the OpenSSL development package for your build target, for
example `pkg-config` and `libssl-dev` on Debian/Ubuntu.

Deploy:

```sh
target/release/bambu-overlay
```

## Usage

Log in once:

```sh
bambu-overlay login
```

Run the overlay server:

```sh
bambu-overlay serve
```

Open `http://127.0.0.1:8765/` for the horizontal overlay or
`http://127.0.0.1:8765/vertical` for the vertical overlay.

When the token account has more than one printer, list the available device IDs:

```sh
bambu-overlay devices
```

Select a printer in the overlay with the `device` query argument:
`http://127.0.0.1:8765/?device=<DEVICE_ID>` or
`http://127.0.0.1:8765/vertical?device=<DEVICE_ID>`. If the argument is
missing or does not match a returned printer, the overlay uses the first printer
from the configured device list.

The browser uses server-sent events from `/api/current-print/events`. The server
emits after MQTT messages and at least once per second. While serving, the
overlay does not poll Bambu Cloud current-print or task APIs; it builds state
from the configured device catalog plus MQTT reports.

The current device catalog is available as JSON at `/api/devices`. It includes
known device metadata and device-specific layout paths. It includes a video path
only when the service has a validated explicit video endpoint or a successful
local startup video probe for that device. Access codes are never included in
this response.

Useful commands:

```sh
bambu-overlay serve --bind 0.0.0.0:8765
bambu-overlay serve --cloud-device 00M123456789012
bambu-overlay serve --local-device 192.168.1.50,12345678,Office
bambu-overlay serve --local-device 192.168.1.50
bambu-overlay serve --local-device 192.168.1.50,12345678,Office --local-device 192.168.1.51,87654321,Garage
bambu-overlay serve --video-device 192.168.1.50
bambu-overlay serve --video-device 192.168.1.50 --video-device 192.168.1.51:6001
```

Configuration is handled with command-line options. Use `--help` on any command
to see the available options. `serve` reads the access token and API base from
the token file in cloud mode and only exposes runtime settings such as
`--bind`, `--token-file`, `--timeout`, `--cloud-mqtt`,
`--local-device`, `--cloud-device`, and `--video-device`.
`--local-device`, `--cloud-device`, and `--video-device` can be repeated.

## Local devices

To add printers directly over LAN MQTT, configure them with `--local-device`:

```sh
bambu-overlay serve --local-device <HOST[:MQTT_PORT]>[,<ACCESS_CODE>[,<NAME>]]
```

`HOST` is the printer LAN address, and `ACCESS_CODE` is the LAN access code shown
on the printer. Startup connects to the printer's local MQTT TLS port and uses
the device certificate common name as the device ID before MQTT authentication.
The MQTT port defaults to `8883`. If `ACCESS_CODE` is omitted, startup looks up
the matching Bambu Cloud `/bind` entry when a token is available. Otherwise
startup fails. Use an empty field when omitting the code but setting a name, for
example `--local-device <HOST>,,<NAME>`. Repeat
`--local-device` for multiple printers.

Hybrid mode is automatic. `serve` calls Bambu Cloud `/bind` only when it needs
device data from it. If a token file exists and no `--cloud-device` or
`--local-device` is provided, `/bind` is used as the cloud device catalog. If any
`--cloud-device <DEVICE_ID>` entry is provided, that explicit list is the
complete cloud device catalog and `/bind` is not used for enumeration.

`--cloud-device` entries are id-only. Standalone cloud devices still require a
Bambu Cloud token for the MQTT UID lookup and MQTT authentication. Local devices
with complete access codes do not trigger `/bind`; local devices missing an
access code and explicit cloud video devices without an access code look up
`/bind` only when that code is actually needed.

To run without any Bambu Cloud API calls, provide only `--local-device` entries
that include access codes.

Select a local printer the same way as cloud printers:
`http://127.0.0.1:8765/?device=<DEVICE_ID>`.

## Video

A1 and P1 series printers can expose their camera as MJPEG at
`/api/video.mjpeg`:

```sh
bambu-overlay serve --video-device 192.168.1.50
```

`--video-device` accepts a printer LAN IP address or hostname, optionally
followed by `:PORT` and `,ACCESS_CODE`, for example
`--video-device 192.168.1.50:6000,12345678`. Repeat it once per printer when
serving multiple cameras. The printer video server uses port `6000` when no port
is specified. `serve` probes each explicit
`--video-device` endpoint at startup, reads the device ID from the printer
certificate common name, and fails if that device is not present in the known
device catalog. Known devices include cloud `/bind` devices when enumeration is
active, plus explicit `--cloud-device` and `--local-device` options. For cloud
devices, `--video-device` provides the LAN camera endpoint and the access code
can be provided on `--video-device` or come from `/bind` metadata. For local
devices, the access code comes from the matching `--local-device` entry or
`--video-device` entry.

For local devices, `serve` probes `<HOST>:6000` at startup. If it can complete a
Bambu device TLS handshake and the printer certificate common name matches the
local device ID, that endpoint is added automatically. No camera access code is
sent during startup video probes. `--video-device` remains useful for cloud
devices and for overriding or adding camera endpoints explicitly.

Select a camera with `/api/video.mjpeg?device=<DEVICE_ID>`. Without `device`,
the first printer from the configured device list is used. For each selected
device, `bambu-overlay` tries the configured video endpoints with that device ID
as TLS SNI. The printer certificate common name is the device serial number, so
`bambu-overlay` uses the certificate to reject mismatched endpoints before
sending the camera access code. It also remembers mismatched endpoint/device
pairs it discovers while probing, then remembers the endpoint that successfully
streams frames for the rest of the process.

The video connection uses `native-tls` with only Bambu's BBL CA certificate
trusted for this transport. The TLS backend verifies the certificate chain,
certificate validity, signatures, and handshake. Hostname verification is
disabled because some printer firmware serves CN-only certificates; after the
TLS handshake, `bambu-overlay` checks that the certificate common name matches
the requested device ID before sending the camera access code.

Only one upstream video connection to the printer is opened. Multiple OBS or
browser clients connected to the same `/api/video.mjpeg?device=<DEVICE_ID>`
stream share that connection, and the printer connection is closed after the
last MJPEG client disconnects.

## systemd

An example service unit is available at
`examples/systemd/bambu-overlay.service`. Adjust the `User`, `Group`,
`ExecStart`, and token file path for your host before installing it.

The example stores the token at `/var/lib/bambu-overlay/token.json` and runs as
the unprivileged `bambu-overlay` user. On systemd versions that support
`StateDirectory=`, systemd creates `/var/lib/bambu-overlay` with the correct
owner when the service starts.

If you create the service user and state directory manually, keep the directory
private and writable only by that service account:

```sh
sudo useradd --system --home-dir /var/lib/bambu-overlay --shell /usr/sbin/nologin bambu-overlay
sudo install -d -o bambu-overlay -g bambu-overlay -m 0700 /var/lib/bambu-overlay
```

Create the token as that user so the resulting file is owned correctly:

```sh
sudo -u bambu-overlay /usr/local/bin/bambu-overlay login --token-file /var/lib/bambu-overlay/token.json
sudo chmod 0600 /var/lib/bambu-overlay/token.json
```
