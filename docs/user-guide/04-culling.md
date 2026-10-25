# Culling

Culling is choosing the keepers. Darkslide is built so you can do it with one hand on the keyboard: move with the arrows, and rate, label or flag with single keys. Each key can be changed in Settings ▸ Shortcuts.

## Views and moving around

- `G` shows the grid, `E` (or `↩`) the viewer. `⌃⌘F` goes full screen.
- `←` `→` move to the previous or next photo; `↑` `↓` move a row in the grid or in a filmstrip with several rows. Hold `⇧` to extend the selection.
- `Space` zooms to 100% at the cursor and back. `⌥⌘0` fits, `⌥⌘1` shows actual size, `⌘+` / `⌘-` zoom.
- `B` hides the filmstrip. Drag the line beside it to show more rows or columns of thumbnails, or right-click a thumbnail to move the filmstrip to the left or right.
- The size slider in the filmstrip's filter bar, or **View ▸ Larger / Smaller Filmstrip Thumbnails** (`⌃⌘=` / `⌃⌘-`), makes the filmstrip's thumbnails bigger or smaller. Larger thumbnails load sharper previews.
- View menu aids: Clipping Warning (`⌥⌘J`), Focus Peaking (`⌥⌘P`), Loupe (`⌥⌘L`) and Navigator (`⌥⌘N`). The same Clipping and Focus peaking switches are in the histogram panel (see [The histogram panel](06-develop.md#the-histogram-panel)).

![The grid. Ratings, colour labels and pick or reject flags show on each thumbnail.](../screenshots/grid.png)

## Ratings, labels and flags

| Key | Does |
|---|---|
| `1` – `5`, `0` | Rate 1 to 5 stars; clear the rating |
| `6` `7` `8` `9` `-` | Red, yellow, green, blue or purple label (orange and pink exist but have no key until you assign one) |
| `P` / `X` / `U` | Pick, reject, unflag |

**Photo ▸ Auto-Advance After Rating** moves to the next photo after each rating. Ratings and labels apply to every selected photo.

## Comparing

- **Multi-view:** select several photos (`⌘`-click or `⇧`-click in the filmstrip) and the viewer shows them all, as large as they fit: two side by side, four as 2×2, up to twelve. The focused photo is outlined, and it is the one you zoom and edit. Turn this off with **View ▸ Show Selection in Viewer**.
- **Survey** (`N` or `⌥⌘S`): the selected photos side by side. Press `X` to take the weakest out until only the keepers are left.
- **Before / after** (`Y`) splits the viewer between the original and your edit. **Pin for compare** (`K`) keeps a photo on screen while you move through the others.

![Multi-view: four selected photos, the focused one outlined.](../screenshots/multiview.png)

![Survey: one photo already taken out with X.](../screenshots/survey.png)

## Burst stacks

A burst is a run of frames shot in quick succession. Darkslide can show each burst as one stack, so you cull the moment once instead of frame by frame.

- **Stack bursts:** tick the **Stack bursts** checkbox beside the filmstrip's thumbnail size, or choose **View ▸ Stack Bursts**. It is on by default. Each burst then shows as one stack in the filmstrip and the grid, and the arrow keys step over it. Untick it to see and walk every frame.
- `S` expands or collapses the focused stack. With stacking off, `S` turns it on, with every burst collapsed.
- **Which frame is the cover:** the frame with the highest star rating wins, so rating a frame above the cover makes it the cover at once. Rejected frames are never the cover (unless every frame is rejected). When several frames share the top rating, the one you chose wins; if you haven't chosen, the AI's pick from the last AI Cull (then its quality score) decides; otherwise the earliest frame. With nothing rated, every frame ties.
- **The burst strip:** click a stack (or its badge) in the filmstrip and its frames open in a strip just above the filmstrip, highest rated first, each with its stars. Click a frame, or use `←` `→`, to see it in the viewer; the rating and flag keys still apply to it.
- The cover is marked in the strip: **Your pick** or **AI pick** when it broke a tie, so you can see whose choice it was and change it.
- **Use as Cover** (the button, a frame's context menu, or `↩`) is offered on the frames tied for the top rating. On the others the strip says what it takes, for example "Rate it ★★★ or higher to make it the cover".
- `Esc`, a click outside the strip or a click on the badge again closes the strip.
- **Your choice is kept:** it is saved with the session, so it is the same on every Mac that opens it. If a frame is later rated higher, that frame becomes the cover, but your choice is remembered and comes back when the ratings tie again. If the burst changes (an import or a delete), the choice stays while its frame is still in the burst. Survey starts on your chosen cover too.

## AI Cull

**Adjustments ▸ AI Cull** (`⇧⌘K`) scores every photo for sharpness, exposure and closed eyes, and groups near-duplicates, all on this Mac. Use the scores to filter, then make the final call yourself.

When AI Cull rejects a photo, it says why in a short note, such as "Eyes closed" or "Duplicate of DSC00001.ARW: that frame is sharper". The note shows as **AI reject** in the info panel, in the viewer and in the reject badge's tooltip. Changing the flag yourself clears it.

## Voice culling

Turn on **View ▸ Voice Commands** (`⌃⌘V`) and say what to do: "pick", "reject", "three stars", "next", "next 3", "go to photo 12", "reject the next 3", "rate the next 5 three stars", "show picks" or "undo 3 times". Say "what can I say?" for every phrase. Speech is recognised on this Mac. Settings ▸ Voice adds your own phrases (for example "keeper" for Pick).

## Finishing a cull

- **Photo ▸ Move Rejected to Trash** moves rejects into the session's Trash folder. **Restore from Session Trash** brings them back.
- **Photo ▸ Move Picked and Rated to Selects…** moves the keepers into Selects.
- **Photo ▸ Delete Rejected…** asks before it deletes anything, and never touches a card or the archive.

## Smart folders, filters and metadata

- **Smart Folders** (Library tab) collect photos by exact rules on rating, label, flag and keyword. `⌘0` shows all photos and `⌘1` – `⌘5` show only that many stars.
- **Filters** narrow the browser to picks, rejects, unflagged photos and more.
- **Metadata** (Metadata tab): title, caption, headline, creator, credit, copyright, location, instructions and usage terms (IPTC Core), and **Keywords**, for one photo or the whole selection. Fields that differ across the selection show as "Mixed" and are kept unless you change them.
