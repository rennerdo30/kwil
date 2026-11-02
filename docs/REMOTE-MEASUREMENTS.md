# Hosted remote desktop measurements

KW-30 uses `.github/workflows/remote.yml` on a disposable Ubuntu hosted runner.
It runs on relevant PR paths and `workflow_dispatch`. The job is informational
(`continue-on-error`): no byte or rectangle threshold blocks merging. Probe
failures stay visible in step logs; they are not zero-valued measurements.

The standard-library Python client starts TigerVNC's Xvnc (`Xtigervnc` on Ubuntu),
connects to its loopback-only RFB socket, and runs Kwil on that isolated display.
No RDP connection or GUI launch occurs on the owner's PC. The runner window must
be mapped to produce VNC pixels, so this hosted-only launch deliberately omits
`--hidden`. All long-lived child PIDs are recorded, bounded and closed.

The launcher supplies `VNCDESKTOP`, because starting Xvnc alone does not construct
an application's session environment. Settings retain `render.remote = "auto"`.
The existing `frame-harness` diagnostic must report `session_detected=true
remote=true blink=false` after focusing the owned window. No forced remote
override or synthetic in-process input is used. The built-in frame script is off.

After initial pixels settle, the client measures idle, one typed character,
one wheel notch, opening a file through the running instance, and post-action
idle. Typing is checked by saving and comparing exact bytes; the IPC-open check
uses the existing first-content trace. Save/setup traffic is excluded from action
samples. Each sample has a fixed duration, so late work is also visible in the
following idle sample. These are individual samples, not statistical budgets.

Artifacts retain `result.json`, every rectangle in `rectangles.json`, process
logs, the isolated profile, and lossless PNGs reconstructed from received pixels.
The Actions summary contains the numbers and commit SHA. Logs start with that SHA.

## Meaning of the counters

- `update_bytes`: server-to-client RFB FramebufferUpdate messages, including the
  4-byte message and 12-byte rectangle headers and encoded payloads.
- `rectangles`, `raw_pixels`, `copy_rectangles`, `copy_pixels`: sums received by
  this client, including overlapping areas and repeated updates.
- `other_server_bytes`: Bell/clipboard messages, counted separately.
- The client requests only **Raw + CopyRect, 32bpp**. This gives transparent
  accounting and a reproducible uncompressed sample; it is not the bandwidth
  expected with Tight/ZRLE, video codecs or TLS. TCP/IP overhead, connection
  setup, framebuffer requests and input traffic are excluded.
- The client keeps one incremental request outstanding. It never forces full
  refreshes between actions. Initial framebuffer transfer is setup traffic.
- Xvnc can compare/coalesce pixels. A wire rectangle is not an app present, zero
  wire traffic does not prove zero presents, and a CopyRect is reported only if
  actually received. Do not infer SPEC's one-copy/one-new-line contract from
  the renderer's source or from a small byte count.

## Still requiring other machines or instrumentation

The optional Windows/RDP job is skipped. Hosted Windows feasibility has not been
demonstrated here: this package has no validated headless FreeRDP per-action
byte/rectangle counter or disposable RDP user/session provisioning. Claiming that
RDP is technically impossible on hosted Windows would exceed the evidence.

Remaining acceptance: actual RDP transport measurements, session switching,
representative compressed VNC codecs, physical host/client behavior, and WAN
latency/bandwidth. A separate host/client VM pair or real machines can provide
these without ever loopback-connecting RDP to the owner's desktop. The original
KW-30 drawing budgets remain unqualified until those measurements exist.

Protocol: [RFC 6143, RFB](https://www.rfc-editor.org/rfc/rfc6143.html).
Server options: [TigerVNC Xvnc manual](https://tigervnc.org/doc/Xvnc.html).
