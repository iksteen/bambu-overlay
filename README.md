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
```

Configuration is handled with command-line options. Use `--help` on any command
to see the available options. `serve` reads the access token and API base from
the token file and only exposes runtime settings such as `--token-file`,
`--timeout`, `--refresh-seconds`, `--mqtt-host`, `--mqtt-port`, and `--no-mqtt`.

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
