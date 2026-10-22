# The Mac ↔ iPad link (`ds-remote`)

The link lets a paired device (the planned iPad app, or today the `ds-remote` CLI) browse the session open on the Mac, download its files, and send photos and edits back. It runs on the LAN and over a tailnet. The plan is in [IPAD_PLAN.md](IPAD_PLAN.md) ("Mac ↔ iPad link"); this page is the protocol as built.

## Pieces

| Where | What |
|---|---|
| `crates/ds-remote` | server, client library, `ds-remote` CLI (`src/bin/ds-remote.rs`, logic in `cli.rs`) |
| `crates/ds-ffi/src/remote.rs` | `RemoteServer` (start/stop, pairing, status, sessions changed) and `remote_devices` / `remote_revoke` / `remote_set_write` |
| `app/Sources/DarkslideKit/Remote/` | `RemoteLinkModel` (Settings ▸ Devices, the toolbar indicator), `DNSSDAdvertiser` (Bonjour), `QRCode` |

The server is **off by default**. It binds only when the user turns on "Allow iPad connections" and stops when it is turned off or the app quits. The app serves the open session; when a device changes it, the app lists it again.

## Security

- **Identity.** Each device makes an Ed25519 key once, with a self-signed certificate for it, in its state directory (`identity.key`, mode 0600, and `identity.crt`; the Mac's is `<Application Support>/Darkslide/Remote/`). What gets pinned is the key's **fingerprint**: BLAKE3 of the 32-byte public key.
- **Transport.** TLS 1.3 only (rustls with the ring provider), mutual authentication, Ed25519 signatures only. There is no CA. Each side checks the other's key, parsed from its certificate, against its pin:
  - the client accepts only the Mac key it paired with;
  - the server accepts a paired key, or any key **while a pairing window is open** (that connection can only pair). An unpaired key with no window open fails the TLS handshake before any request is read.
  - Session resumption is off on both sides (no tickets, no session cache), so every connection is a full handshake through that check: a revoked key, or one let in while pairing was open, can't come back on a resumed session.
- **Address filter.** The server listens on every interface (binding to the Tailscale interface alone breaks when Tailscale reconnects) and drops, before TLS, any peer outside loopback, RFC 1918, link-local, the Tailscale CGNAT range `100.64.0.0/10` (which is also the ISP carrier-grade-NAT range, so on a CGNAT uplink or hotspot other subscribers can reach the port; keys are still pinned, and the pairing window is the exposed surface) and IPv6 `fc00::/7` (which holds Tailscale's `fd7a:115c:a1e0::/48`) and `fe80::/10`. IPv4-mapped IPv6 addresses are judged as IPv4. Nothing is ever forwarded or opened on the router. Its limits:
  - It is a first line only; the pinned keys are the real control. Behind NAT hairpinning or a port forward the source becomes the router's LAN address, and `100.64.0.0/10` is also carrier-grade NAT (a Mac on such an ISP or a phone hotspot sees other subscribers there), and `fc00::/7` admits any ULA.
  - **Global IPv6 addresses are refused, even on the same LAN.** A device that reaches the Mac by its global IPv6 address (`.local` names and Bonjour often resolve to it, and iOS prefers IPv6) is dropped. The iPad client should connect to the Mac's IPv4, link-local, ULA or tailnet address; allowing peers in the Mac's own on-link prefixes is a possible later change.
- **Limits.** Silent, trickling or squatting sockets can't hold the slots a paired device needs:
  - at most 8 handshakes in progress, 2 from one address (an IPv6 `/64` counts as one address, so rotating addresses within it doesn't help), each with an absolute 10-second deadline. When an address is at its limit its oldest handshake is dropped; when all 8 are taken, the oldest from the most represented address is. Drops are budgeted (a burst of 8, then 2 a second; past that the newcomer is turned away), so a flood can delay a real handshake but not keep dropping it;
  - at most 8 paired connections; one with no request for 2 minutes is closed;
  - at most 2 unpaired connections (keys let in while pairing is open), 1 from one address (IPv6 `/64`). They may say hello once and make one pairing attempt, and must pair within 30 seconds of the handshake (requests don't extend it); they are closed when the pairing window closes or expires, when their pairing attempt fails, or on any request out of order (`pair_finish` before `pair_start`). A connection that pairs becomes a paired one.
  - Refused peers are logged at most once a minute.
- **Pairing.** Settings shows a QR code and a 6-digit code, valid once, for five minutes:
  - The QR code is `darkslide-pair://<host>:<port>?fp=<Mac key fingerprint>&t=<128-bit token>&c=<code>`. A client that scans it pins the Mac's key before connecting and uses the token as the secret.
  - A client that types the code accepts the Mac's key during this one handshake.
  - Either way, the secret is never sent. Both sides run **SPAKE2** (Ed25519 group) over it, then each sends a confirmation: a BLAKE3 MAC, keyed from the SPAKE2 key, over both TLS key fingerprints. A device in the middle (which must show each side its own key) can't produce matching confirmations without the secret, and gets one guess per attempt; three wrong confirmations close the window. A malformed SPAKE2 message is no guess: it closes the connection but doesn't count toward the three.
  - On success the Mac stores the device as **read-only** and the client stores the Mac's key.
- **Paired devices** live in `devices.json` (written atomically) and are read on every request, so a revoke or a change of write access applies to a connected device's next request.
- **Write access** (push photos, push edits) is off for a new device and granted per device in Settings ▸ Devices (`ds-remote grant` for the CLI server).
- **RAW parsing in the app:** a pushed original is read by the RAW parser (`rawler`) inside the app, for its preview and metadata. A panic in a request is caught (that request fails with `internal` and the connection closes, freeing its slot); an abort in the parser is not recoverable, which is why pushing needs the per-device write grant.
- **Pairing crypto:** SPAKE2 comes from RustCrypto's `spake2` crate, which has not had an independent audit. The confirmation is bound to both TLS keys, so a flaw there could at worst allow online guessing of the code, still limited to three tries per window.

## Framing

One TLS connection carries request/response pairs, one at a time. A frame is:

```
u32 big-endian header length | header (JSON) | body (header.body bytes)
```

The header is a JSON object with an `op` tag and a `body` length (at most 4 MiB, the chunk size; headers at most 1 MiB; the sender refuses a bigger one with an `internal` error, so a long listing is paged, never cut). Hashes are BLAKE3 as 64 hex digits. The first request must be `hello`.

## Messages (protocol version 2)

| Request (`op`) | Fields | Answer |
|---|---|---|
| `hello` | `version`, `name` | `welcome {version, name, paired, write}`; another version gets `error version` and the connection closes |
| `pair_start` | `secret: code \| token`, `spake` (SPAKE2 message A, hex) | `pair_challenge {spake}` (message B) |
| `pair_finish` | `confirm` | `paired {confirm, name}` (the Mac's confirmation) |
| `list_sessions` | | `sessions [{id, name}]` |
| `list_photos` | `session`, `after?` (a previous page's `next`), `limit?` (default 200, at most 500) | `photos [{rel, size, hash?, missing, variants [{ordinal, variant_id, name, sidecar_hash, modified_ms}]}]`, `next?`; photos are sorted by `rel`, a page also ends at about 512 KiB of header, and `next` (the last `rel` sent) is absent on the last page |
| `stat` | `session`, `file` | `file {size, hash}` |
| `read` | `session`, `file`, `hash`, `offset`, `len` | `data` + body; `error changed` if the file no longer has `hash` |
| `push_status` | `session`, `name`, `size`, `hash` | `push_state {have}`, or `pushed` if the session already has this content |
| `push_chunk` | `session`, `name`, `size`, `hash`, `offset` + body | `push_state {have}`, then `pushed {rel, duplicate}` when complete |
| `push_sidecar` | `session`, `rel`, `ordinal`, `based_on?`, `base?` (the base sidecar's exact text), `sidecar`, `modified_ms` | `sidecar_merged {outcome, hash, kept_ordinal?}` + body (the Mac's sidecar now) |

`file` is `{"kind": "original" | "preview", "rel": "Capture/…"}` or `{"kind": "sidecar", "rel": …, "ordinal": n}`. Only photos in the session's index are served (originals, previews and sidecars alike), by session-relative path (validated like every session path: no `..`, no hidden components). A preview is the original's embedded JPEG.

Errors are `error {code, message}` with `code` one of `version`, `unpaired`, `read_only`, `not_found`, `changed`, `hash_mismatch`, `bad_request`, `pairing_closed`, `pairing_failed`, `internal`. `unpaired` (a revoked device) and pairing failures close the connection.

## Transfers

Both directions use `ds-import`'s scheme (`crates/ds-remote/src/transfer.rs`):

- Chunks (4 MiB) go to a hidden `.partial` next to the destination; its length is where a resumed transfer starts (`read` at that offset, or `push_status` → `have`).
- When complete, the partial is fsynced, re-read with the page cache bypassed and checked against the sender's BLAKE3 (`ds_import::verify_hash`). A mismatch discards the partial and the transfer starts again (the client retries twice); a match is placed with a no-replace rename.
- Nothing is replaced. A pull refuses a different file at the destination. A push that names an existing file with other content is placed as `name-2.ext`; content the session already has is reported as a duplicate. Pushed originals go to `Capture/From <device name>/` and are added to the session's index. Folders opened in place take no pushed originals (their originals are the user's own folder).
- A push is at most 4 GB, and one device's unfinished pushes at most 8 GB in all. A push's `.partial` left untouched for a day is removed the next time the session is served.
- The server keeps only file hashes between requests (per size and modification time) and the bytes of the last preview or sidecar it served, so pulling thousands of previews doesn't grow its memory.
- The CLI joins only valid session paths from the Mac onto its destination, and reads at most 16 MB into memory (`fetch`, for sidecars).
- A file that changes on the Mac during a pull answers `changed`; the client starts that file again.

## Sidecar merge

`push_sidecar` carries the edited sidecar, the hash of the Mac sidecar it was edited from (`based_on`) and, when the client still has it, that sidecar's exact text (`base`). The Mac uses `base` only if it hashes to `based_on`. The whole merge runs under the session's lock: in the app, that is the app's own open session, so the Mac's sidecar is read, compared and written with no app write in between. The Mac applies, per variant (`crates/ds-remote/src/merge.rs`):

1. **Unchanged:** the push equals the Mac's sidecar.
2. **Fast-forward:** the verified base is the Mac's sidecar as it is at that moment: the push is taken. A claimed `based_on` without a matching base is never a fast-forward.
3. **Field merge:** otherwise each field is merged against `base`: a field only one side changed takes that side's value; keywords merge as sets (both sides' additions and removals). Rating, colour label and pick flag that both sides changed take the newer write (`modified_ms` against the Mac sidecar's modification time).
4. **Conflict:** both sides changed the adjustments differently. The newer edit wins on the variant, and the other is kept as a **new variant** ("Edited on <device>" or "Edited on this Mac"), created before the variant is changed, so nothing is lost.

Without a verified base, every differing field counts as changed on both sides. Only adjustments, rating, colour label, pick flag and keywords are taken from a push; the rest of the sidecar stays the Mac's.

**The app's side.** A variant a device changed is marked "unseen" while the session lock is still held. It stays unseen until the app has the reloaded edits in its model (the app reloads the session about a second after a push; `remote_mark_seen` is called after the reload has put the edits in the model and flushed saves queued meanwhile). If the app saves adjustments over an unseen device edit (a slider move or an Undo before or during the reload), the device's adjustments are first kept as a variant "Edited on <device>". So neither side's edit is lost to the other. Device writes carry a generation number, and the app's edits snapshot (`load_all_adjustments_json`) reports the generation it includes; `remote_mark_seen` clears only device edits up to that generation, so a push that lands while a reload is under way stays unseen until the next reload. One known redundancy: after a conflict the device won (the Mac's edit kept as "Edited on this Mac"), an app save from its pre-reload state also keeps the device's edit as "Edited on <device>" and writes the Mac's edit back onto the variant, so two variants hold the Mac's edit. Nothing is lost; delete the extra one.

**Listing.** `list_photos` reads the session's index under the lock (the app keeps it current; the server doesn't sync it) and reads and hashes the sidecars after releasing it, so a device listing in a loop doesn't hold up the app.

**Undo.** Device pushes are not steps in the app's Edit ▸ Undo history. What protects the Mac's work is the variant kept on a conflict ("Edited on this Mac") and on a save over an unseen device edit; to go back, use that variant. A write device can still replace the Mac's edit with a genuine fast-forward (it edited the Mac's current sidecar), as any edit would: write access is for devices you trust with the session.

## The CLI

`ds-remote` (`cargo run -p ds-remote --bin ds-remote -- …`) is both ends:

```
ds-remote --state ./mac   serve --session ~/Pictures/Darkslide/Trip --pair          # prints the code and QR text
ds-remote --state ./ipad  pair 'darkslide-pair://mac.local:47474?fp=…&t=…&c=…'      # or: pair mac.local:47474 123456
ds-remote --state ./ipad  ls                       # sessions
ds-remote --state ./ipad  ls Trip                  # photos and variants
ds-remote --state ./ipad  pull Trip ./out --originals
ds-remote --state ./mac   grant <device fingerprint prefix>
ds-remote --state ./ipad  push Trip ./new/DSC0001.ARW
ds-remote --state ./ipad  push-sidecars Trip ./out  # sends sidecars edited under out/Sidecars
ds-remote --state ./mac   devices | readonly <fp> | revoke <fp>
```

`pull` writes originals to `out/<rel>`, previews to `out/Previews/<rel>.jpg` and sidecars to `out/Sidecars/<rel>.<variant>.json` (with the pulled copy under `out/Sidecars/.base/`, which `push-sidecars` sends as the base). Interrupt `pull` or `push` and run it again: it resumes.

## Tested here, and not

Loopback end-to-end tests (`crates/ds-remote/tests/e2e.rs`) cover pairing by code and by QR token, wrong codes and the attempt limit, an unpaired key refused, no resumption after the pairing window closes, a non-private address dropped (through the `PeerPolicy` seam), silent sockets timing out without taking the paired slots, the connection cap, the unpaired per-address cap (over both loopback stacks) and deadline, out-of-order and malformed pairing messages, listing and pulling, a dropped connection resumed, a corrupted chunk retried, a file changing mid-pull, pushes with read-only refusal, resume, duplicates and no-replace naming, the merge cases (including an unverified base), revocation taking effect at once, and stopping. `merge.rs` has property tests. `crates/ds-ffi/src/remote.rs` tests the app editing its open session while a device pushes.

Not testable on one machine, behind seams or left for the phase 4 iPad work:
- a real pull across a tailnet from another machine (manual: `ds-remote serve` on the Mac, the CLI on another tailnet machine);
- Bonjour discovery from another device (`DNSSDAdvertiser`; tests register on this Mac only);
- the iPad client itself (Network framework `sec_protocol_options` verify block with the same pinning, SPAKE2, the offline queue).
