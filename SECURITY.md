# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue:

- through GitHub's private vulnerability reporting (the repository's **Security** tab ▸ **Report a vulnerability**), or
- by email to **[SECURITY CONTACT — maintainer to fill in before publishing]**.

Include what you found, how to reproduce it (a minimal file or session helps), the Darkslide version (Darkslide ▸ About Darkslide) and your macOS version. You'll get an acknowledgement within a week. Please give us a reasonable time to ship a fix before disclosing it.

## Scope

Darkslide reads untrusted files and manages irreplaceable ones, so these matter most:

- **File safety:** anything that can lose, corrupt or overwrite originals, sidecars, sessions or the archive (imports from cards, sidecar and session writes, archive and backup hand-off, moving and trashing).
- **Parsing untrusted input:** RAW, DNG, JPEG, HEIC and XMP files, Capture One sessions, presets, camera profiles and other files Darkslide opens.
- **Network use:** model downloads (each file is pinned by SHA-256) and the optional services you turn on yourself.

Supported versions: the latest release only.
