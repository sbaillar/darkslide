# Developing

Edits are never applied to the original file. They are stored as settings in the sidecar and rendered by the develop pipeline on the GPU, so you can change or undo them at any time (`⌘Z` / `⇧⌘Z`).

**Adjustments ▸ Reset All Adjustments** (`⇧⌘R`) takes the photo back as shot, with no adjustments, in one undo step. It does the same as the **Original** row in Styles & Presets.

![A live edit: warmer white balance, more exposure, a crop and a straighten, with the histogram.](../screenshots/live-edit.png)

## Tool tabs

Tools live in tabs across the top of the tools pane, like Capture One. Tabs that don't fit go in a » menu.

- The **+** button adds or removes any tool in one click. Each panel's **⋯** menu removes or moves it, and panels can be dragged into any order.
- **Tools ▸ Customize Tools…** (`⇧⌘T`) arranges tabs, creates your own (for example a "Portraits" tab with just the tools you use for portraits) and saves **workspaces**.
- The **Tools** menu lists every tool by tab; choosing one shows it.
- **Adjustments ▸ Edit All Selected** applies each change to every selected photo.

![A custom "Portraits" tab next to the built-in tabs.](../screenshots/tools-custom.png)

## The histogram panel

The histogram sits under the tool tabs, above the panels, whichever tab is open. Under it, three checkboxes share one line: **Clipping** (clipped highlights in red and shadows in blue on the image), **Focus peaking** and **Soft proof**. When the tools pane is too narrow for one line, they go on two centred lines. The soft proof options (profile, rendering intent, black point compensation and gamut warning) appear below them only while **Soft proof** is on.

## Light

- **Exposure:** exposure, contrast, brightness and saturation.
- **High Dynamic Range:** highlights, shadows, whites and blacks.
- **Levels** and **Curve** (RGB and per channel).
- **Presence:** texture, vibrance and dehaze.

## White balance and colour

- **White Balance:** as shot, presets, temperature and tint, or the picker (`W`): click something that should be neutral.
- The picker's loupe, a magnifying circle beside the cursor, shows exactly what you are about to sample, with a readout below it. It opens zoomed out, showing about 60 pixels across, so you can see where you are picking. Scroll over the loupe to zoom in or out.
- **Color Balance** wheels (master, shadows, midtones, highlights) and the **Color Editor** for individual colour ranges. Saturation is in Exposure and vibrance in Presence.
- **Camera profiles** (looks): Luxtable Standard, Neutral and Vivid, or an imported Adobe DCP profile.
- `I` is a colour readout picker; click to pin a readout.

![The white balance picker's loupe and readout.](../screenshots/wb-loupe.png)

## Looks, styles and presets

The **Styles & Presets** panel holds one-touch looks: Portrait, Cinematic, film negatives and slides, faded matte, cross-processed, vintage, three black-and-whites, moody, bright and airy, golden hour, dreamy glow, pop, HDR detail, tilt-shift miniature and halation film.

- Each look previews on the photo as you hover, applies in one click as one undo step, and keeps an **Intensity** slider; every setting it made stays editable.
- The **Original** row at the top is the photo as shot, with no adjustments. Hover it to compare, and click it to go back to it in one undo step, so you don't have to undo through every style you tried. `⌘Z` brings your edit back.
- After a click, the tiles stay visible, dimmed, while they refresh with the new look.
- Save your own settings as presets, or add looks with plug-ins.

**Edit ▸ Match Total Exposures** (`⌥⇧⌘M`) and **Match White Balance to Focused Photo** even out a set of photos.

## Detail and lens

- **Sharpening**, **Noise Reduction** (ISO-adaptive, plus AI Denoise; see [AI tools](07-ai.md#ai-denoise-and-ai-upscale)) and **Clarity** (clarity and structure).
- **Lens Correction:** lens profiles, distortion, chromatic aberration, and vertical and horizontal keystone. **Vignetting** corrects or adds a vignette.
- **Effects:** an AI **lens blur** (background blur from an on-device depth model; click to set the focus, choose a bokeh shape, or use a tilt-shift band), glow, film grain and halation.

## Layers and masks

The **Layers** tab adds local adjustments. Each layer has its own mask and most of the global tools.

- **Brush** and **erase** strokes, **linear** and **radial gradients**, **luminance** and **colour range** masks. Shapes combine by adding, subtracting or intersecting, with feather and invert. `[` and `]` change the brush size. You can bind keys to the mask tools in Settings ▸ Shortcuts.
- **AI masks** for the sky, the subject, and people: a person, or parts of them (face, skin, eyes, teeth, hair, clothes). The sky and people-part models are downloaded the first time you use them.

## Heal, clone and remove

In the **Heal & Clone** panel (Details tab), heal and clone spots such as sensor dust. The **Remove** tool fills whatever you paint over from its surroundings: paint over a distraction, release, and it's gone. A downloaded on-device model (LaMa) does the fill when it is installed; a built-in fill is used until then.

## Crop and straighten

- `C` opens the crop tool, with Free, Original, 1:1, 4:5, 3:2 and 16:9 presets. Drag the handles; `↩` applies, `Esc` cancels, `X` swaps the orientation.
- `R` is the straighten tool: drag along something that should be level.
- `⌘[` / `⌘]` rotate left or right.

![The crop tool at 4:5, mid-drag.](../screenshots/crop-tool.png)

## Variants, history and soft proofing

- **New Variant** (`⌘D`) makes another version of a photo without duplicating the file.
- **History** (**View ▸ Show History**) lists the photo's edit steps, newest on top. Click a step to go back to it, hover to preview it.
- **Soft Proof** (`⇧S`, or the **Soft proof** checkbox under the histogram) shows the photo through any installed ICC profile, such as a printer and paper, and can mark out-of-gamut colours.

![Full-resolution detail at 100% (Space or ⌥⌘1).](../screenshots/zoom-1to1.png)
