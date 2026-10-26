# Luxtable website

A single static page (`index.html`) with no build step, no trackers and no scripts. Fonts (Outfit, Inter, JetBrains Mono) load from Google Fonts.

## Before publishing

The repository does not exist yet, so every GitHub link uses the placeholder `https://github.com/OWNER/luxtable` (see the TODO comment at the top of `index.html`). Replace it once, in all links, with:

```sh
sed -i '' 's/OWNER/<github-user-or-org>/g' site/index.html
```

There is no JavaScript on the page; the links are plain `href`s.

## Publish with GitHub Pages

1. Push the repository to GitHub.
2. Settings > Pages > Build and deployment > Source: **GitHub Actions** (or "Deploy from a branch" with a `gh-pages` branch that contains the contents of `site/`).
3. For the Actions route, use the stock "Deploy static content to Pages" workflow with `path: site`.

To preview locally: `python3 -m http.server -d site` and open http://localhost:8000.

Images are in `img/`. `img/logo.png` is a copy of `docs/brand/luxtable-logo.png`; the screenshots are resized copies of `docs/screenshots/` (synthetic demo data; retake them with `scripts/screenshots.sh`).
