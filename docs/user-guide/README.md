# Darkslide User Guide

How to import, cull, develop, export and archive your RAW photos with Darkslide for macOS. Read it here, one chapter per file, or as the [PDF](../Darkslide-User-Guide.pdf).

1. [Welcome to Darkslide](00-introduction.md#welcome-to-darkslide): the ideas behind it and the main window
2. [Install and first run](01-install.md#install-and-first-run): download or build, first-run setup, the RAW helper and updates
3. [Sessions](02-sessions.md#sessions): new, open and close, folders opened in place, missing files
4. [Importing from a memory card](03-import.md#importing-from-a-memory-card): options and what makes it safe
5. [Culling](04-culling.md#culling): views, ratings, comparing, burst stacks, AI Cull and voice
6. [Library, Find, People and Map](05-library.md#library-find-people-and-map): every session's photos together
7. [Developing](06-develop.md#developing): tool tabs, the histogram, light and colour, styles, layers and crop
8. [AI tools](07-ai.md#ai-tools): Edit with words, AI Optimize, AI Changes, Denoise and Upscale
9. [Copy and apply](08-copy-apply.md#copy-and-apply)
10. [HDR and panorama](09-hdr-panorama.md#hdr-and-panorama)
11. [Exporting](10-export.md#exporting): recipes and Google Photos
12. [Archive, Immich and Amazon Photos](11-archive.md#archive-immich-and-amazon-photos): folder or object storage archive, Immich albums
13. [Plug-ins](12-plugins.md#plug-ins): plug-ins and the optional integrations
14. [The Mac ↔ iPad link](13-ipad-link.md#the-mac--ipad-link)
15. [Coming from other apps](14-migrate.md#coming-from-other-apps): Capture One, Lightroom Classic, Aperture and iPhoto
16. [Settings](15-settings.md#settings)
17. [Troubleshooting](16-troubleshooting.md#troubleshooting)
18. [Keyboard shortcuts](17-shortcuts.md#keyboard-shortcuts) (appendix)

## Building the PDF

The PDF is built from the numbered `NN-*.md` files, in order, with [pandoc](https://pandoc.org) and [WeasyPrint](https://weasyprint.org):

```sh
brew install pandoc weasyprint
scripts/user-guide.sh              # writes docs/Darkslide-User-Guide.pdf
scripts/user-guide.sh --diagrams   # also re-renders diagrams/*.mmd to img/*.svg (needs Google Chrome)
```

- `guide.css` styles the PDF: the cover, the contents, chapter pages and the shortcut tables.
- Screenshots come from `docs/screenshots` (taken in demo mode with `scripts/screenshots.sh`); diagrams from `diagrams/*.mmd`, rendered to `img/*.svg`.
- A new chapter is a new `NN-name.md` file starting with one `#` heading. Link to other chapters as `NN-name.md#heading-id` (GitHub's heading ids); the build turns those into links inside the PDF.
