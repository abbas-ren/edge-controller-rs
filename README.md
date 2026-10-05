# edgecontroller

Rust device-controller service for Gen3, Gen4, and Gen5 board mapping, relay
control, firmware programming, RTOS capture, and backend heartbeats.

## Configuration

The service reads the legacy `key=value` login configuration. Required keys are
`server_ip`, `http_port`, `ws_port`, and `gen`. Optional keys are `bind_port`
(default `8888`) and `iface_name` (default `eth0`). See
`config/login.toml.example` for an example.

Hardware bindings are loaded from `/etc/dev-controller/hardware.json` by
default. `files/etc_dev-controller_hardware.json.sample` documents the expected
shape. State mappings remain compatible with the legacy files under `/var/log`.

Useful environment variables:

- `DEV_CONTROLLER_BIND`: API socket address; defaults to `0.0.0.0:8888`.
- `DEV_CONTROLLER_INTERFACE`: overrides the configured network interface.
- `DEV_CONTROLLER_HARDWARE`: absolute hardware-policy path.
- `DEV_CONTROLLER_TOKEN`: optional 32-256 character bearer token. Configure it
	whenever the API is reachable outside a trusted network.

Run `edgecontroller --help` for CLI options. Generation and RTOS functionality
must be enabled explicitly with `--enable-gen3`, `--enable-gen4`,
`--enable-gen5`, and `--enable-rtos`. `--check` validates configuration and
persisted state without opening hardware or network listeners.

Gen3 and Gen4 use the same relay/GPIO FlashWriter sequence. Gen5 uses the
administrator-installed X5H script and its built-in CPLD power interface, so it
does not require relay or GPIO bindings.

Gen3/Gen4 startup requires `--vid-pid <VID:PID>`. The optional
`--relay-serial-number <iSerial>` is accepted only with `--vid-pid`. When the
serial is omitted, exactly one matching USB relay must be connected so its
iSerial can be resolved. The hardware policy
assigns one `gpio` and one relay `channel` per board; relay identity is runtime
configuration rather than static policy.

Firmware requests configure resting `gpioDefaultLevel` and `relayDefaultLevel`
values independently as `HIGH` or `LOW`; both default to `LOW`. With those
defaults, normal boot holds GPIO LOW and leaves the relay de-energized at LOW.
Entering download mode preserves the hardware sequence: relay HIGH, wait two
seconds, GPIO HIGH, wait two seconds, then relay LOW.

## Logging

Structured console logging is enabled by default. `--log-level` accepts
`trace`, `debug`, `info`, `warn`, or `error`; `--log-file` adds rolling file
output. `--log-network` and `--log-stream` enable more detailed diagnostics for
those subsystems. Request payloads, credentials, and backend message contents
are not logged.

## HTTP API

The API and Prometheus endpoint share the controller listener (port `8888` by
default). When `DEV_CONTROLLER_TOKEN` is set, send
`Authorization: Bearer <token>` on every request.

Operational endpoints:

- `GET /health`, `GET /ready`, `GET /status`, `GET /metrics`
- `GET /swagger.json`, `GET /docs`
- `POST /confirmation`

Hardware endpoints:

- Relay and mapping: `/relay`, `/relay/status`, `/relay/config`,
	`/relay/identity`, `/relay/delete`, `/devCon/delete`, `/mapping/entry`,
	`/reboot-device`
- Firmware: `/ipl`, `/ipl-mode`, `/ipl-mode/default`, `/ipl/remove`
- Gen5: `/gen5/tty_entry`, `/gen5/power`
- RTOS capture: `/rtos/start`, `/rtos/end`

Swagger UI at `/docs` contains request schemas, success statuses, common error
responses, and authentication details for every route.

## Metrics

`GET /metrics` returns Prometheus exposition data:

- `edgecontroller_up`: `1` while the process is serving metrics.
- `edgecontroller_http_requests_total`: requests by method, route, and status.
- `edgecontroller_http_request_duration_seconds`: request latency histogram by
	method and route.
- `edgecontroller_firmware_flash_total`: completed flashes by generation and
	outcome.
- `edgecontroller_firmware_flash_duration_seconds`: flash duration histogram by
	generation.
- `edgecontroller_websocket_connected`: `1` while connected to the backend.
- `edgecontroller_websocket_events_total`: bounded connection, reconnect,
  heartbeat, and received-frame events for the backend WebSocket.
- `edgecontroller_registration_attempts_total`: backend registration attempts by
	bounded outcome (`accepted`, `rejected`, or `transport_error`).
- `edgecontroller_hardware_operations_total`: hardware API outcomes by bounded
	operation name.
- `edgecontroller_hardware_operation_active`: `1` while the serialized hardware
	lease is held, including detached firmware jobs.
- `edgecontroller_mapping_entries`: persisted mapping counts by `usb` or `gen5`.
- `edgecontroller_active_captures`: retained RTOS capture session count.

An auxiliary exporter also starts on the CLI metrics port (default `8081`) for
compatibility with existing monitoring deployments. Labels use bounded route
and status values and do not contain device identifiers.

## Development

The standard validation sequence is:

```sh
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-features
cargo build --release --all-features
RUSTDOCFLAGS="-D missing_docs" cargo doc --no-deps
git diff --check
```

Hardware-facing tests are intentionally deterministic and do not require live
USB, GPIO, UART, backend, or relay devices. Deployment validation should still
exercise those integrations on approved hardware.

## Deployment

Build and install the optimized binary and systemd unit as root:

```sh
cargo build --release --all-features
install -m 0755 target/release/edgecontroller /usr/bin/edgecontroller
install -m 0644 systemd/dev-con.service /etc/systemd/system/dev-con.service
systemctl daemon-reload
systemctl enable --now dev-con.service
```

The unit defaults to Gen4 and FTDI VID:PID `0403:6001`. Override the relay
selector, enabled generations, RTOS capture, or other CLI options in
`/etc/default/edgecontroller`:

```sh
EDGE_CONTROLLER_ARGS="--enable-gen4 --relay-serial-number AB0OFAFX --vid-pid 0403:6001 --log-file /var/log/edgecontroller.log"
```

Install the login configuration at `/etc/config/login.cfg`, the hardware policy
at `/etc/dev-controller/hardware.json`, and firmware under
`/var/lib/dev-controller/firmware`. The service runs with filesystem hardening
but retains host device access for GPIO, USB relays, and serial interfaces.
Shutdown has no forced timeout because accepted firmware jobs must retain their
hardware lease until protocol cleanup completes.
