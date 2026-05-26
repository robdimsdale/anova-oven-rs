# Pico ↔ Server Transport Security — Options & Recommendation

Status: **Investigation only. No code written, nothing committed.**
Date: 2026-05-15
Scope: link between `crates/anova-oven-pico` (RP2040 Pico W) and
`crates/anova-oven-server` (we own both ends).
Related: [`docs/pico-ota.md`](pico-ota.md) — OTA brief. Read both together;
this document supersedes that doc's "no TLS → signing mandatory" framing by
explaining the actual transport-security options.

This is a self-contained pickup brief. A future agent or human should not need
to re-derive the facts below.

---

## 1. TL;DR

- Today the Pico talks to the server over **plain HTTP, no TLS** (`reqwless`,
  `embassy-net` DHCPv4).
- We **own both ends and the Pico talks to nothing else**, so we fully control
  the protocol — no backward-compat constraints.
- Securing the channel is **feasible and worthwhile** (the Pico controls a
  heating appliance; control commands + telemetry currently travel in clear).
- **TLS 1.3-only is the correct constraint.** Not because TLS 1.2 is impossible
  in embedded Rust generally, but because the only no_std TLS client that fits
  RP2040 — `embedded-tls` (already used by `reqwless`'s `embedded-tls`
  feature) — is **TLS 1.3-only by design**. Since we own the server we just
  configure/require 1.3; zero compat cost.
- Two viable architectures:
  1. **TLS 1.3 with PSK** (pre-shared key) — stays on standard HTTPS/`reqwless`.
  2. **Noise protocol** (e.g. `noise-rust` / `snow`) — lighter, no certs, no
     clock; arguably the better fit for a closed both-ends-owned link.
- **Independent of transport, ed25519 OTA image signing is still mandatory**
  (see `pico-ota.md`). TLS/Noise secures the *channel*; signing secures the
  *payload at rest in the DFU partition*. They solve different problems; we
  want both for OTA.

---

## 2. Why TLS 1.3-only (the embedded-Rust reality)

The practical no_std TLS client on RP2040 is **`embedded-tls`**
(https://github.com/drogue-iot/embedded-tls). `reqwless` already integrates it
behind its `embedded-tls` feature, so enabling HTTPS is mostly a feature flip
plus the design work below. `embedded-tls` deliberately implements **only TLS
1.3** — it never implemented 1.2.

Alternatives and why they lose here:

- **rustls** — now has a no_std path, but pulls `alloc` + a crypto provider and
  is heavy for a Cortex-M0+ with 264 KB RAM. Not worth it.
- **mbedtls (C bindings)** — works but large and awkward inside this
  Embassy/no_std setup.

Conclusion: if we do TLS, it is **TLS 1.3-only on both ends**, and we configure
`anova-oven-server` to require 1.3 (can refuse anything lower — only client is
the Pico).

---

## 3. RP2040-specific gotchas (apply to TLS *and* Noise)

These are the things that make this non-trivial; the implementer must plan for
all three.

1. **No hardware RNG on RP2040.** Both `embedded-tls` and Noise need a
   `CryptoRng`. RP2040 has **no TRNG** (RP2350/Pico 2 W does — relevant if we
   ever migrate). You must seed a CSPRNG (e.g. ChaCha20) from the ring
   oscillator `ROSC` random-bit source. ROSC entropy quality is debatable, so
   this is a real security design decision, not boilerplate. Get this wrong and
   the whole channel is compromised regardless of protocol.

2. **No real-time clock.** The Pico has no battery-backed RTC; wall-clock time
   at boot is unknown. **X.509 certificate validation (`notBefore`/`notAfter`)
   cannot be done normally.** This is the single biggest reason to prefer
   PSK/Noise over cert-based TLS here. Workarounds if certs are insisted on:
   pin a private CA/trust anchor via `include_bytes!` (like the existing cyw43
   blob) and disable/relax time validation — workable but strictly weaker and
   more code than a pre-shared-key scheme.

3. **RAM budget.** TLS 1.3 records are up to 16 KB; `embedded-tls` needs
   separate read + write record buffers (~32 KB+ total). Context: the firmware
   already allocates a **16 KB** HTTP RX buffer
   (`HTTP_RX_BUF_LEN = 16384` in [`src/api.rs`](../crates/anova-oven-pico/src/api.rs#L15),
   used at [`src/api_client.rs:523`](../crates/anova-oven-pico/src/api_client.rs#L523)).
   Current usage is ~128 KB static + 32 KB heap of 264 KB (re-measured 2026-05-26
   after the picoserve `/health` endpoint landed; see `pico-ota.md` §2), so there is headroom,
   but TLS buffers must be **explicitly budgeted** — this ties directly into
   `pico-review.md` finding 5.3 ("no resource budget"). Noise framing is far
   smaller (no 16 KB record requirement), which is a concrete RAM advantage.

---

## 4. The Noise Protocol Framework

### What it is

The **Noise Protocol Framework** is a toolkit for building secure
point-to-point channels from a small set of building blocks (a DH function, a
cipher, and a hash) composed into named **handshake patterns**. Unlike TLS it
is **not** a single negotiated protocol with certificates and a PKI — you pick
one pattern and both ends are hardcoded to it. It is the transport security used
by WireGuard, the Lightning Network, WhatsApp, and many IoT links.

Relevant handshake patterns for our case:

- **`Noise_NN`** — ephemeral-only, no authentication (not useful for us alone).
- **`Noise_NK`** — client knows server's static public key; server
  unauthenticated client. Good if only the server needs proving.
- **`Noise_XX`** — mutual authentication, static keys exchanged during
  handshake. Flexible.
- **`Noise_KK`** — both static keys known to each other ahead of time. Smallest,
  strongest for a **closed both-ends-owned system like ours**: bake the server's
  static public key into the firmware, register the Pico's static public key on
  the server. Mutual auth, no certs, no clock, minimal handshake.

`Noise_KK` (or `Noise_IK`) is the natural choice here.

### Links

- Official spec: https://noiseprotocol.org/noise.html
- Project site: https://noiseprotocol.org/
- Pattern explorer / explainer: https://noiseexplorer.com/
- Rust impl `snow`: https://github.com/mcginty/snow
  (https://crates.io/crates/snow) — has `no_std` support; widely used.
- Rust impl `noise-rust` / `noise-protocol`:
  https://github.com/blckngm/noise-rust
- Background (why it exists, design): "The Noise Protocol Framework" talk/notes
  linked from https://noiseprotocol.org/

### Noise vs TLS 1.3 — pros / cons for *this* project

| Dimension | TLS 1.3 (`embedded-tls` + `reqwless`) | Noise (`snow`/`noise-rust`, e.g. `KK`) |
|---|---|---|
| Standardization | Industry standard; interops with anything | Bespoke; both ends must agree on pattern (fine — we own both) |
| Server-side support | `anova-oven-server` uses standard Rust TLS (rustls/axum) — easy | Server needs a Noise impl + framing layer — more custom work |
| Certificates / PKI | Cert mode needs PKI + **a real clock** (we have none); PSK mode avoids this | **No certs, no clock ever** — static keys only |
| Clock dependency | Cert mode: yes (blocker); PSK mode: no | No |
| RNG dependency | Yes (ROSC CSPRNG) | Yes (ROSC CSPRNG) — same constraint |
| Code size / RAM | Larger; 16 KB record buffers ×2 | Smaller; no 16 KB record requirement, tiny handshake |
| Reuses existing HTTP stack | **Yes** — keep `reqwless`/HTTP request shape, just HTTPS | **No** — need a framed transport over `embassy-net` TCP; HTTP semantics must be re-layered or dropped |
| Mutual auth | Possible (mTLS / PSK) but more setup | Built-in with `KK`/`XX`/`IK`; natural |
| Maturity in embedded Rust | `embedded-tls` is the de-facto choice; known quantity | `snow` no_std is solid but you own more of the integration |
| Migration to RP2350 later | Fine | Fine |

**Reading of the trade-off:** if the priority is "keep speaking HTTPS / minimize
server-side change / use the well-trodden path," choose **TLS 1.3 with PSK**. If
the priority is "smallest, simplest, clock-free, RAM-frugal secure link for a
closed system we fully own," choose **Noise (`KK`/`IK`)** — at the cost of
giving up HTTP framing and writing more of the server side ourselves.

---

## 5. Recommendation

1. **Do secure the channel** — the control link to a heating appliance should
   not be cleartext, and we have no compat constraints.
2. **Keep ed25519 OTA image signing regardless** — it is orthogonal and
   mandatory for safe OTA (see `pico-ota.md`); do not let "we have TLS now"
   remove the signing requirement.
3. **Default recommendation: TLS 1.3 with PSK.** Rationale: minimal server-side
   change (standard Rust TLS in `anova-oven-server`), reuses `reqwless`/HTTP,
   avoids the clock problem (PSK, not certs), and `embedded-tls` is the
   best-trodden embedded path. Accept the ~32 KB RAM cost and budget it
   explicitly.
4. **Strong alternative if RAM/code size or simplicity dominate, or if we are
   willing to invest in custom server transport: Noise `Noise_KK`.** Bake the
   server static public key into the firmware; register the Pico's static key
   server-side. No clock, smaller footprint, mutual auth by construction.
5. **Whichever path: solve the RNG first.** A vetted ROSC-seeded ChaCha20
   CSPRNG is a prerequisite for *both* options and is the highest-risk security
   primitive on RP2040. Validate it before building on it.

### Suggested sequencing

1. Build and review the ROSC → CSPRNG entropy source in isolation.
2. Decide TLS-PSK vs Noise-KK (a one-page decision record; default TLS-PSK).
3. Stand up the chosen handshake against `anova-oven-server` with a trivial
   echo endpoint; measure real RAM/flash delta on hardware.
4. Migrate the existing `reqwless`/api_client traffic onto the secure channel.
5. Re-confirm the RAM budget against `pico-review.md` 5.3 with the new buffers.
6. Only then integrate with the OTA work in `pico-ota.md` (signed image over the
   now-encrypted channel).
