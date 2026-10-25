# Install and first run

## Requirements

- A Mac with Apple silicon, running macOS 26 or later.
- To build it: the Xcode Command Line Tools (Swift 6.2) and Rust 1.90 or newer. Xcode itself isn't needed.

## Download

Download `Darkslide-X.Y.Z-macos-arm64.zip` from the project's GitHub releases, unzip it and move Darkslide.app to your Applications folder. The app is ad-hoc signed, not notarised: the first time, right-click Darkslide.app and choose **Open**. From then on Darkslide keeps itself up to date (see [Updates](#updates)).

## Build from source

You can also build Darkslide from its source code. In Terminal:

```sh
git clone https://github.com/sbaillar/darkslide.git
cd darkslide
DS_SKIP_MODELS=1 scripts/build-app.sh --run
```

This builds `dist/Darkslide.app` and opens it. You can then copy it to your Applications folder.

- `DS_SKIP_MODELS=1` builds faster by leaving out the AI Denoise model (building it needs Python 3.13 and a download). Without it, AI Denoise shows as unavailable. Leave the variable out for a full build.
- `scripts/make-signing-identity.sh` creates a local signing identity once, so macOS remembers the permissions you give Darkslide across rebuilds.

## Try it without your photos

Demo mode opens sample sessions of synthetic photos and simulates imports and archiving. It never reads or changes your own files:

```sh
dist/Darkslide.app/Contents/MacOS/Darkslide --demo
```

Every screenshot in this guide was taken in demo mode.

## First run

The first time Darkslide opens, a short setup sheet walks you through a few steps. **Skip for Now** keeps the defaults, and everything can be changed later in Settings (`⌘,`).

1. **Welcome:** what Darkslide does.
2. **What did you use before?** Choose Darkslide, Lightroom Classic or Capture One. This only moves panels, keyboard shortcuts and a few names to where your hands expect them (Settings ▸ Familiar).
3. **Where should sessions live?** The folder where new sessions are created. Pick your fastest disk.
4. **Archive (optional):** tick **Archive sessions automatically** and choose a folder (for example on a NAS) that sessions are copied to in the background. It stays off until you turn it on.
5. **Amazon Photos (optional):** a folder the Amazon Photos app backs up. This step only appears while the Amazon Photos integration is on (see [Optional integrations](12-plugins.md#optional-integrations)).

## Permissions

macOS asks before Darkslide can read removable volumes, network volumes or folders such as Pictures. Allow these for the drives you keep photos on. Voice commands ask for microphone and speech recognition access the first time you turn them on.

## The RAW helper

Darkslide's own decoders read DNG (including iPhone ProRAW), Sony ARW, Canon CR2, Nikon NEF and Pentax PEF. Most other RAW formats, and the camera data that ARW, NEF and CR2 files need to develop, come from the optional **RAW helper**, a separate program you install only if you need it. Without it, those photos import and cull from their embedded previews, and HDR and panorama merges aren't available.

Open **Settings ▸ RAW ▸ Download RAW Helper**, or click **Get RAW Helper** on a photo that needs it. Darkslide checks the download against the fingerprint built into the app before it installs it. **Remove RAW Helper**, in the same place, takes it out again.

## Updates

Darkslide looks for a new version shortly after it starts and then about once a day. When there is one, a small notice says so; it never interrupts you with a dialog in the middle of culling. **Darkslide ▸ Check for Updates…** checks straight away.

- The update sheet shows what's new. **Download & Restart** downloads the update, checks it, quits the normal way (pending changes are saved first) and reopens on the session and photo you were on, with the same selection, view and tool tab.
- **Download** installs it the next time you quit, **Later** reminds you another time, and **Skip This Version** stops the reminders for that version.
- The version it replaces is kept in Darkslide's Application Support folder, in **Previous Version** (only the last one), so you can go back by dragging it to Applications.

**Settings ▸ Updates** turns **Check for updates automatically** off, and has **Include beta versions**: betas are early builds of the next version, for trying new features before they're released, and may have bugs. Once that version is released, you are offered the release. The check sends only the app's version to GitHub.
