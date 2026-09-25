# Transport and wire-schema evaluation

Contributor note, researched 2026-09-18. Revisits the three transports described
in [Daemon-to-client transports](client-transports.md) and asks two questions
that turn out to have different answers:

1. **Transport.** Is WebSocket still right for the daemon-to-client surface, or
   should it move to gRPC/Connect, SSE, or WebTransport?
2. **Schema.** Should the wire shape stay hand-written in three places, or be
   generated from one definition?

**Answers: keep WebSocket, and generate the types.** They are independent
decisions. Every schema option worth having can be adopted without touching the
transport, and the transport alternatives do not fix the drift problem anyway.

## The constraints this was tested against

These came from the product, not from preference. An option that fails one of
them is out, however good the rest of it looks.

- One port, single binary, no sidecar proxy in the deployment.
- Plain HTTP on a trusted LAN, HTTPS behind the user's own reverse proxy, and
  Tailscale Funnel. All three, same build.
- Clients: React dashboard including iOS Safari on a phone, native Rust TUI,
  and external scripts that `curl` the documented REST API.
- `/sessions/{id}/live-ws` is genuinely bidirectional: frames down, keystrokes
  and control messages up, up to 60fps with a 16ms floor.
- Payload CPU is not a bottleneck. JSON encoding costs tens of microseconds
  against a ~1ms `tmux capture-pane` fork and a 16ms frame floor, and frames
  already ride a connection-lifetime deflate stream.

Two consequences of the deployment constraints do most of the work below.

**Browsers do not speak cleartext HTTP/2.** The HTTP/2 FAQ: "no browser supports
HTTP/2 unencrypted"
([http2.github.io/faq](https://http2.github.io/faq/)). On a plain-HTTP LAN
address the daemon is serving HTTP/1.1 to the dashboard, full stop. Anything
that needs HTTP/2 end to end is unavailable in exactly the deployment we
advertise first.

**A plain-HTTP LAN origin is not a secure context.** MDN lists the potentially
trustworthy origins as `https`, `wss`, `file`, `127.0.0.0/8`, `::1/128`, and
`localhost`
([MDN, Secure contexts](https://developer.mozilla.org/en-US/docs/Web/Security/Defenses/Secure_Contexts)).
`http://192.168.1.5:8080` is not on that list, so any API gated on a secure
context is simply absent there.

**Tailscale Funnel is a TCP/TLS proxy.** Its documented limitations: "Funnel can
only listen on ports `443`, `8443`, and `10000`" and "Funnel only works over
TLS-encrypted connections", with TLS terminated on the tailnet node
([Tailscale KB 1223](https://tailscale.com/kb/1223/funnel)). No UDP is offered,
so no QUIC and no HTTP/3.

---

# Part 1: transport

## gRPC-Web

Does not do what the live pane stream needs, and this is not a gap that is
closing.

- Supported modes, from the project README: "gRPC-web currently supports 2 RPC
  modes: Unary RPCs and Server-side Streaming RPCs (NOTE: Only when
  `grpcwebtext` mode is used.)"
  ([grpc/grpc-web](https://github.com/grpc/grpc-web)). Binary `grpcweb` mode
  supports unary only.
- Client and bidi streaming are not planned. The streaming roadmap: "We don't
  plan to support client-streaming via Fetch/upload-streams... half-duplex bidi
  streaming won't be supported via Fetch/streams either", and "We have no plan
  to support full-duplex streaming over WebSockets (over TCP or HTTP/2)"
  ([streaming-roadmap.md](https://github.com/grpc/grpc-web/blob/master/doc/streaming-roadmap.md)).
- **Proxy:** the reference deployment needs one. "gRPC-web clients connect to
  gRPC services via a special proxy; by default, gRPC-web uses Envoy." Envoy,
  grpcwebproxy, APISIX and Nginx are listed.
- **But a Rust server can skip the proxy.** `tonic-web` "enables tonic servers
  to handle requests from `grpc-web` clients directly, without the need of an
  external proxy", wrapping services in a `GrpcWebLayer`
  ([docs.rs/tonic-web](https://docs.rs/tonic-web/latest/tonic_web/)). Its own
  docs restate the ceiling: "Currently, grpc-web clients can only perform
  `unary` and `server-streaming` calls", with no websocket transport support.
  (Note for anyone searching: `hyperium/tonic` now redirects to
  `grpc/grpc-rust`.)
- The one historical exception proves the point. `@improbable-eng/grpc-web`
  "provides a built-in websocket transport that can support
  client-side/bi-directional streaming RPCs", which is to say the only way
  anyone got bidi gRPC into a browser was to tunnel it over a WebSocket. That
  repo is "in maintenance mode", recommends migrating to grpc/grpc-web, and has
  had no pushes since 2023-09
  ([improbable-eng/grpc-web](https://github.com/improbable-eng/grpc-web)).

## Connect

Better than gRPC-Web, and genuinely proxy-free against a Rust server, but it
still cannot carry the live pane stream from a browser.

- The Connect protocol itself supports every RPC shape. The spec: "Bidirectional
  streaming requires HTTP/2, but the other RPC types also support HTTP/1.1"
  ([Connect protocol](https://connectrpc.com/docs/protocol/)).
- In a browser that is theory only. `connect-es` throws on any non-server-stream
  method before it builds a request body:

  ```ts
  if (method.methodKind != "server_streaming") {
    throw "The fetch API does not support streaming request bodies";
  }
  ```

  ([connect-web/src/connect-transport.ts](https://github.com/connectrpc/connect-es/blob/main/packages/connect-web/src/connect-transport.ts)).
  The FAQ puts it more gently: "web browsers have some limitations with regard
  to client streaming" ([Connect FAQ](https://connectrpc.com/docs/faq/)).
- The underlying limit is the Fetch API. A `ReadableStream` body requires
  `duplex: "half"`
  ([MDN, RequestInit](https://developer.mozilla.org/en-US/docs/Web/API/RequestInit)),
  and per Chrome's own write-up it "Doesn't work on HTTP/1.x", requires HTTPS,
  triggers a CORS preflight, and is half duplex only
  ([Chrome, Streaming requests with fetch](https://developer.chrome.com/docs/capabilities/web-apis/fetch-streaming-requests)).
  Shipped in Chromium 105; Safari exposes streams on `Request` but does not
  allow them with `fetch`. So even the half-duplex version is Chromium-only and
  needs HTTP/2, which a plain-HTTP LAN origin cannot give us.
- **Proxy:** none needed in front of a Rust server, provided the Rust server
  speaks Connect or gRPC-Web itself. A proxy is only required when the backend
  speaks standard gRPC over HTTP/2 and nothing else. Connect's FAQ: "Most of
  Google's gRPC implementations don't support gRPC-Web, so you must run a proxy
  like Envoy."
- **Rust server implementation:** yes, and it is real. `connectrpc` is "A
  Tower-based Rust implementation of the ConnectRPC protocol", serving Connect,
  gRPC and gRPC-Web clients, with axum integration documented, supporting all
  four RPC shapes (full-duplex bidi requiring HTTP/2). It claims the full
  conformance suite, 3,600 server and 6,872 client tests
  ([connectrpc/connect-rust](https://github.com/connectrpc/connect-rust)).
  Status is pre-1.0 with an API that "may shift in 0.x"; MSRV 1.88. crates.io
  shows 0.9.0 published 2026-08-25, first published 2025-10-14, 6.78M downloads
  with 4.28M in the recent window
  ([crates.io/crates/connectrpc](https://crates.io/crates/connectrpc)). Maturity
  verdict: young, actively developed, credibly tested, not yet stable-API.

So Connect could carry REST and the runtime stream. It could not carry the live
pane stream from a browser without a second transport beside it, and a second
transport is what we already have.

## WebTransport

Fails the LAN constraint outright, and fails Funnel outright.

- **Browser support has actually arrived.** MDN marks it "Baseline 2026: Newly
  available", and browser-compat-data gives Chrome 97, Firefox 114, Safari 26.4,
  with Safari on iOS mirroring desktop Safari
  ([MDN, WebTransport](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport),
  [BCD api/WebTransport.json](https://github.com/mdn/browser-compat-data/blob/main/api/WebTransport.json)).
  That is very recent: an iPhone that has not taken Safari 26.4 has no
  WebTransport at all, which for a phone-first dashboard is a live concern for
  some time yet.
- **It cannot run on a plain-HTTP LAN page.** MDN flags WebTransport as
  available "only in secure contexts (HTTPS)", and `http://192.168.x.x` is not
  one. Independently, the spec requires the URL scheme itself to be `https`: "If
  url's scheme is not `https`, throw a `SyntaxError` exception"
  ([W3C WebTransport](https://w3c.github.io/webtransport/)).
- **It cannot run over Funnel.** WebTransport is HTTP/3 over QUIC, which is UDP.
  Funnel is TCP on three fixed ports.
- **Certificate requirements, verbatim from the spec:** "the certificate MUST be
  an X.509v3 certificate as defined in [RFC5280], the key used in the Subject
  Public Key field MUST be one of the allowed public key algorithms, the current
  time MUST be within the validity period of the certificate... and the total
  length of the validity period MUST NOT exceed two weeks." The allowed
  algorithm list "MUST include ECDSA with the secp256r1 (NIST P-256) named
  group... It MUST NOT contain RSA keys." Hash verification matches only
  `sha-256`. `serverCertificateHashes` "is only supported for transports using
  dedicated connections" and "cannot be used with `allowPooling`". BCD shows the
  option supported in Chrome 100, Firefox 125, Safari 26.4.
- Practically that means the daemon would mint a P-256 certificate, rotate it at
  least fortnightly, and hand the SHA-256 hash to the page out of band before
  connecting. The page it would hand it to is served over plain HTTP on the LAN,
  which is not a secure context, so the API is not there to call. The chain
  breaks before the certificate question matters.
- One more thing to watch even where it is possible: Chrome 147 puts
  WebTransport under
  [Local Network Access](https://developer.mozilla.org/docs/Web/Security/Defenses/Local_network_access)
  restrictions (BCD `WebTransport.local_network_access`, marked experimental).

Unconfirmed: whether Chrome or Safari accept an IP-literal authority such as
`https://192.168.1.5:4433/` together with `serverCertificateHashes`. No primary
source found either way. It does not change the conclusion, since the secure
context requirement already rules the LAN case out.

## Server-Sent Events

Credible in principle for the runtime stream, which is server to client only.
Not worth adopting here, for reasons specific to this codebase.

- **Connection limits.** The HTML spec acknowledges the problem without
  quantifying it: "Clients that support HTTP's per-server connection limitation
  might run into trouble when opening multiple pages from a site if each page
  has an `EventSource` to the same domain", and suggests distinct domains, a
  per-page toggle, or sharing one `EventSource` through a shared worker
  ([WHATWG HTML, Server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html)).
  MDN supplies the number: "When not used over HTTP/2, SSE suffers from a
  limitation to the maximum number of open connections... the limit is per
  browser and is set to a very low number (6)... marked as 'Won't fix' in Chrome
  and Firefox... When using HTTP/2, the maximum number of simultaneous HTTP
  streams is negotiated between the server and the client (defaults to 100)"
  ([MDN, Using server-sent events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events/Using_server-sent_events)).
  The 6 is browser behavior documented by MDN, not a normative number in any
  spec. Note it is the HTTP/1.1 connection pool, so it is not specific to SSE: a
  long-lived Connect server-stream over `fetch` consumes the same socket.
- The HTTP/2 escape hatch is not available to us. See the cleartext point above:
  on the plain-HTTP LAN deployment the dashboard is on HTTP/1.1, so a user with
  a handful of dashboard tabs open would be spending their six sockets on
  streams and starving REST.
- **Auth does not fit.** `EventSource` takes exactly one option,
  `withCredentials`, and cannot set request headers
  ([MDN, EventSource()](https://developer.mozilla.org/en-US/docs/Web/API/EventSource/EventSource)).
  `src/server/auth.rs` accepts the token by cookie, query parameter,
  `Sec-WebSocket-Protocol`, or `Authorization: Bearer`, and the bearer path
  exists specifically "used by the PWA, which persists the token in
  localStorage since iOS `start_url` strips the query param on home-screen
  relaunch". That is the one client that would need a header on the stream, and
  it is the one client `EventSource` cannot give a header to. A fetch-based
  reimplementation such as
  [Azure/fetch-event-source](https://github.com/Azure/fetch-event-source)
  restores headers, at the cost of hand-rolling reconnection.
- **Proxy buffering is a real operational footgun.** nginx defaults to
  `proxy_buffering on`, which "receives a response from the proxied server as
  soon as possible, saving it into the buffers", spilling to disk if it does not
  fit. It can be turned off per response by sending `X-Accel-Buffering: no`,
  unless the operator has set `proxy_ignore_headers`
  ([nginx, proxy_buffering](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_buffering)).
  Since our users bring their own reverse proxy, an SSE runtime stream would
  intermittently arrive in batches on somebody's setup, and the bug report would
  be "the dashboard freezes then jumps". WebSocket upgrades are not subject to
  response buffering.
- What SSE would genuinely buy: built-in reconnection with `Last-Event-ID`,
  which maps neatly onto the existing `epoch`/`revision` cursor. That is worth
  noticing, and it is not worth a transport migration to obtain, because the
  same resume can be built on the socket we already have.

## Why WebSocket keeps winning

Not novelty, just fit. It is the only browser transport that is bidirectional,
runs over a single port on HTTP/1.1, works on a plain-HTTP origin, survives a
TLS-terminating TCP proxy, and is available on every iOS Safari anyone still
runs.

- Connection count is not capped the way SSE is. RFC 6455 constrains only
  simultaneous handshakes: "There MUST be no more than one connection in a
  CONNECTING state", and asks clients to serialize pending connections to the
  same host
  ([RFC 6455 section 4.1](https://www.rfc-editor.org/rfc/rfc6455.txt)). Nothing
  limits established sockets to six.
- It already carries auth cleanly through `Sec-WebSocket-Protocol`.
- The deflate stream in `src/server/live_ws.rs` is a connection-lifetime raw
  deflate with a dictionary carried across frames. No request-response transport
  can reproduce that, because the dictionary is what pays for the redundancy
  between consecutive terminal frames.

### Recommendation

**Keep WebSocket for all three surfaces. Do not adopt gRPC/Connect, SSE, or
WebTransport as a replacement.**

Do not justify any future move by serialization cost. Given a 16ms floor and a
1ms fork per capture, replacing JSON with protobuf or Cap'n Proto buys a few
microseconds per frame on a path that is already dominated by tmux and by
the wait. It would buy a smaller wire, but the deflate stream is already
exploiting the frame-to-frame redundancy that is the bulk of the payload, so
even that gain is smaller than a benchmark against uncompressed JSON suggests.
If bandwidth ever does become the complaint, measure the compressed bytes on the
wire before assuming the encoding is the lever.

### What would change this

- The dashboard needing to reach daemons it cannot hold a socket to, for example
  through an HTTP-only corporate proxy. SSE or Connect server-streams degrade
  better there.
- Dropping the plain-HTTP LAN deployment and requiring HTTPS everywhere. That
  puts HTTP/2 on the table, which makes SSE and Connect streaming reasonable for
  the runtime stream, though still not for the live pane.
- WebTransport becoming worth revisiting only if the product also gains a
  QUIC-capable ingress. It will not happen under Funnel.

### Smaller, in-scope idea

`client-transports.md` records that `RUNTIME_PROTOCOL_VERSION` is enforced by
exact equality at handshake, which is what blocks putting remote daemons on the
runtime stream. WebSocket has a standard answer to that: the client offers a
list of subprotocols and the server picks one it supports (RFC 6455). Today
`Sec-WebSocket-Protocol` carries the auth token, so using it for version
negotiation as well needs a deliberate convention rather than a naive second
value. Worth considering alongside the fallback design that doc already
sketches.

---

# Part 2: schema and codegen

## The problem to solve

`src/daemon/live.rs` and its siblings are the Rust-side source of truth, and the
two Rust ends cannot drift because they compile against the same types. The
dashboard declares the shape a third time in `web/src/hooks/useLiveTerminal.ts`
and nothing makes that a compile error. `the_wire_is_what_both_clients_parse`
pins the encoding so a rename fails a test, and the TypeScript side is not even
structurally parallel: `LiveFrame` there is a flat interface, while
`LiveServerMessage` in Rust is an internally tagged enum.

The wire also uses the harder end of serde: `#[serde(tag = "type")]` internally
tagged enums, `#[serde(flatten)]` on the meta struct in every `frame` and
`patch`, mixed `camelCase` and `snake_case` fields that cannot be tidied, plus
`default` and `skip_serializing_if`. Any generator that cannot express those
either fails or lies.

## Options

| Option | Source of truth | Types only? | Build cost | Status |
| --- | --- | --- | --- | --- |
| ts-rs | existing Rust types | yes | dev-dep plus `cargo test` | 12.0.1, active |
| typeshare | existing Rust types | yes | external CLI binary | 1.0.5, active, but see below |
| specta | existing Rust types | yes | dep plus exporter crate | v2 in RC since 2023 |
| prost + ts-proto | new `.proto` IDL | yes (`onlyTypes`) | `protoc` on every build | both very active |
| prost + protobuf-es | new `.proto` IDL | yes | `protoc`/`buf` | both very active |
| Cap'n Proto | new `.capnp` IDL | no | `capnp` binary on every build | TS side alpha |
| schemars + json-schema-to-typescript | existing Rust types | yes | dep plus npm step | both active |

### ts-rs, recommended

Derive macro on the types that already exist, no IDL. `#[derive(TS)]` plus
`#[ts(export)]` generates a test, and "When running `cargo test` or `cargo test
export_bindings`, the following TypeScript type will be exported"; `TS::export`
and `TS::export_to_string` are available for non-test use
([docs.rs/ts-rs](https://docs.rs/ts-rs/latest/ts_rs/)).

The deciding detail is serde coverage. Supported attributes, verbatim: "rename,
rename-all, rename-all-fields, tag, content, untagged, skip, skip_serializing,
skip_serializing_if, flatten, default". That is a superset of everything
`live.rs` uses, `flatten` included, and unsupported attributes emit a warning
rather than silently generating a wrong type.

Output is type declarations and unions only. It has no runtime, no transport
opinion, and no view on how bytes reach the browser, so the WebSocket layer is
untouched. crates.io: 12.0.1, last release 2026-01-31, ~5.9M recent downloads;
repo last pushed 2026-08-31, 1.9k stars, not archived.

Costs, honestly:

- A dependency in the main crate. It can be feature-gated with
  `#[cfg_attr(feature = "ts-bindings", derive(TS))]` so shipping builds do not
  carry it, at the cost of one more feature combination, which this repo already
  charges disk for.
- One file per type by default. The generated directory has to be committed and
  checked, and the dashboard has to import instead of declare.
- Drift check in CI is the shape this repo already uses for
  `cargo xtask gen-docs`: run the export, then fail on a dirty tree.

### typeshare, disqualified

It would otherwise be a good fit: CLI over the source tree, no IDL, TypeScript
among its targets, 3k stars, pushed 2026-09-13, CLI 1.0.5 from 2026-01-02. But
its changelog records that it "Now throws an error if `#[serde(flatten)]` is
used, instead of silently generating incorrect types"
([typeshare CHANGELOG, PR #108](https://github.com/1Password/typeshare/blob/main/CHANGELOG.md)).
`#[serde(flatten)]` is on `LivePaneMeta` in every `frame` and `patch` message.
Adopting typeshare means restructuring the wire to suit the tool, on a wire
whose field names are already pinned by deployed clients. That is backwards.

The failure being loud rather than silent is to its credit, and it is worth
remembering if typeshare is ever considered for the REST bodies, which may not
use `flatten`.

### specta, not now

Same ergonomics as ts-rs, derive on existing types, TypeScript exported through
`specta-typescript`, and the maintainers mark the TypeScript exporter stable.
The problem is release status: the latest stable release on crates.io is 1.0.5
from 2023-07-17, and everything since has been `2.0.0-rc.*`, currently rc.25
from 2026-05-07 ([crates.io/crates/specta](https://crates.io/crates/specta)).
docs.rs "latest" still resolves to 1.0.5, which makes the documentation story
confusing. It is the natural choice for a Tauri codebase and carries extra risk
for no extra benefit here.

### Protobuf, wrong shape for this job

`prost` and `prost-build` are mature and enormous (0.14.4, ~133M recent
downloads for `prost`). On the TypeScript side, `ts-proto` can emit declarations
only: "With `--ts_proto_opt=onlyTypes=true`, only types will be emitted, and
imports for `long` and `protobufjs/minimal` will be excluded"
([ts-proto README](https://github.com/stephenh/ts-proto)), and `protobuf-es`
generates plain objects, pairs with Connect optionally rather than
necessarily, and supports the JSON mapping
([bufbuild/protobuf-es](https://github.com/bufbuild/protobuf-es)). Both are
actively released (ts-proto 2.12.4 on 2026-09-15, protobuf-es 2.15.0 on
2026-09-11). So "protobuf types over our existing WebSocket" is technically
available.

It is still the wrong trade here:

- It introduces a second source of truth. The `.proto` file becomes canonical
  and the Rust types become generated, which is a larger change than the problem
  justifies, or else the `.proto` mirrors the Rust by hand and we have recreated
  the drift we are removing.
- `prost-build` "depends on the Protocol Buffers compiler, `protoc`, to parse
  `.proto` files", found on `PATH` or via `PROTOC`, with no bundled copy
  ([docs.rs/prost-build](https://docs.rs/prost-build/latest/prost_build/)). That
  puts a C++ toolchain dependency on every build, including the `cargo build`
  path that today needs neither Node nor anything else, and it means adding
  `protoc` to `nativeBuildInputs` in `flake.nix` and the `.proto` files to
  `commonArgs.src`, whose comment already warns that source filtering keeps only
  `*.rs`, `*.toml` and `Cargo.lock`.
- Protobuf's JSON mapping is not our JSON. Field naming, `oneof` representation
  and default/absence semantics all differ from the current encoding, so
  adoption means either breaking every deployed client or writing the mapping
  layer by hand, which is the exact code being eliminated.

Reconsider only if Connect is adopted for the REST surface, at which point the
IDL pays for two things instead of one.

### Cap'n Proto, no

`capnproto-rust` is healthy (`capnp` 0.27.2, 2026-09-08). The TypeScript side is
not: `capnp-es`, the maintained rework of `capnp-ts`, says "This is an
alpha-quality software. please use at your own risk" and still requires the
`capnpc` binary ([unjs/capnp-es](https://github.com/unjs/capnp-es)). The Rust
side also wants the C++ tool: "You still need the `capnp` binary (implemented in
C++)" ([docs.rs/capnpc](https://docs.rs/capnpc/latest/capnpc/)). All of the
protobuf costs, a less mature TypeScript story, and its headline benefit is
zero-copy decode speed, which the constraints say is not our problem.

### JSON Schema, the reasonable alternative

`schemars` derives a JSON Schema from existing Rust types (1.2.2, 2026-07-27,
~169M recent downloads), and `json-schema-to-typescript` turns that into `.d.ts`
(16.0.0, 2026-08-28, repo pushed 2026-09-07). Same "no IDL, keep the transport"
property as ts-rs, with one extra hop and one extra ecosystem, and the schema
itself is a publishable artifact that external orchestrators could validate
against, which has some appeal given the documented REST API. Take this instead
of ts-rs only if runtime validation on the TypeScript side is wanted, since a
JSON Schema can drive a validator where a bare `.d.ts` cannot.

## What generated types will not fix

Worth being clear, because this is the failure mode that generated bindings
invite. Structural generation proves the dashboard's types match the Rust
types. It does not prove either side agrees about meaning, and several of the
bugs this wire has produced were of that kind: a cursor origin applied twice, a
field published but never read. Keep `the_wire_is_what_both_clients_parse`, and
extend the same pinning to `runtime.rs` and `wire.rs`, which
`client-transports.md` already flags as owed. Consider emitting the test's case
table as JSON fixtures that the Vitest suite parses with the generated types, so
one table serves both sides.

### Recommendation

**Adopt ts-rs for the three wire modules, types only, transport untouched.**
Order of work: `live.rs` first, since it has the most drift history and the
strictest serde usage; then `runtime.rs`; then `wire.rs`, which is the largest
and least urgent. Gate the derive behind a feature so default builds are
unaffected, commit the generated directory, and add an export-and-diff check to
CI in the same shape as the `gen-docs` check. Keep the encoding tests.

---

## Where sources disagree, are ambiguous, or could not be confirmed

- **Connect's browser streaming story is documented inconsistently.** The FAQ
  says the Connect protocol "supports all types of streaming RPCs" and elsewhere
  that browsers have "some limitations with regard to client streaming", while
  `choosing-a-protocol`, `supported-browsers-and-frameworks` and `using-clients`
  do not state the limit at all. The unambiguous statement is in the source: the
  transport throws for any method kind other than `server_streaming`. Trust the
  code.
- **The SSE six-connection limit is not normative.** MDN states it; the WHATWG
  spec only acknowledges that per-server connection limits cause trouble and
  offers workarounds. Attempts to pin the constant in Chromium's
  `client_socket_pool_manager` source were unsuccessful, so it is reported here
  as documented browser behavior rather than a specification guarantee.
- **WebTransport against a bare IP with `serverCertificateHashes`** could not be
  confirmed from any primary source, in either direction. Moot given the secure
  context requirement, but flagged.
- **Funnel and WebSocket.** Tailscale's Funnel documentation lists its
  limitations without mentioning WebSocket either way. That Funnel is a
  TLS-terminating TCP proxy makes an upgrade pass through, and this is how the
  feature is deployed today, but it is inference from the architecture rather
  than a documented guarantee.
- **`tonic-web`'s limitation text** was read from docs.rs, since the README in
  the repository covers setup only. Both belong to the same crate.
- **`connectrpc` crate maturity** rests on the project's own conformance claim
  and on download counts. No independent audit was found, and the API is
  explicitly unstable below 1.0.
