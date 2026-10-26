# The Mac ↔ iPad link

Settings ▸ Devices lets a paired device work with the session open on your Mac. It is off until you turn on **Allow iPad connections**, and it only runs while Luxtable is open.

- **Pair a Device…** shows a QR code and a 6-digit code. The code works once, for five minutes.
- A paired device on the same network or your tailnet can browse the open session and download previews, originals and sidecars. Turn on **Can send photos and edits** to let it send new photos and edits back; edits merge without losing either side's work.
- Transfers are mutually authenticated (TLS 1.3 with pinned keys), resumable and checksum-verified. **Revoke** removes a device at any time.

> [!NOTE]
> The iPad app is planned. Today the `lt-remote` command-line client uses the link; see `docs/REMOTE.md`.

![Settings ▸ Devices, with the link off.](../screenshots/settings-devices.png)
