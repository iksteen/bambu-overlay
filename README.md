# bambu-overlay

Rust rewrite of the Bambu overlay prototype.

The release build is a single deployable binary. HTML, CSS, and browser
JavaScript are embedded at compile time with `include_str!`; no external static
files are needed next to the binary.

## Build

```sh
cargo build --release
```

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

The browser uses server-sent events from `/api/current-print/events`. The server
emits after MQTT messages and at least once per second. `--refresh-seconds`
controls how often the slower Bambu Cloud HTTP data is refreshed.

Useful commands:

```sh
bambu-overlay serve --host 0.0.0.0 --port 8765
bambu-overlay serve --no-mqtt
bambu-overlay serve --video-host 192.168.1.50
```

Configuration is handled with command-line options. Use `--help` on any command
to see the available options. `serve` reads the access token and API base from
the token file and only exposes runtime settings such as `--token-file`,
`--timeout`, `--refresh-seconds`, `--mqtt-host`, `--mqtt-port`, `--no-mqtt`,
and `--video-host`.

## Video

A1 and P1 series printers can expose their camera as MJPEG at
`/api/video.mjpeg`:

```sh
bambu-overlay serve --video-host 192.168.1.50
```

`--video-host` is the printer's LAN IP address or hostname. The printer video
server uses port `6000` by default; override it with `--video-port` if needed.
The LAN access code is fetched from Bambu Cloud on stream startup via the
`dev_access_code` field in the current-print response and is not stored by
`bambu-overlay`.

Bambu printer TLS certificates are not always accepted by strict Rust TLS
validation, so the video connection uses TLS with printer SNI but does not
enforce WebPKI certificate validation. Keep the MJPEG endpoint on a trusted
network.

Only one upstream video connection to the printer is opened. Multiple OBS or
browser clients connected to `/api/video.mjpeg` share that connection, and the
printer connection is closed after the last MJPEG client disconnects.

Video streaming requires a token account with exactly one printer that reports
`dev_access_code`.

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
