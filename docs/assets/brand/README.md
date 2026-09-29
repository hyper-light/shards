# shards logo

Drawn as native SVG on 2026-09-28 from the shards project mark on hyperlight-site
(`components/project-mark.tsx` at `612bc83`). The site mark is line art: three shard
outlines, a faint ridge down each, and a short glint. This version fills them. Each shard
splits along its ridge into a solid face and a lighter one, and the glint is cut through.
The shards float apart at unequal heights and spacings, like the site's shards study, so
they read as a fleet of separate machines rather than one object broken into pieces.

## Files and construction

- `shards-fleet.svg` is the editable monochrome vector master.
- `shards-fleet-light.svg` and `shards-fleet-dark.svg` use the same geometry in the
  sibling brands' theme colors, `#1f2328` and `#f0f6fc`. The README displays these SVGs
  directly.
- `shards-fleet-transparent.png` is a 1008 × 1080 export of the master.
- `shards-fleet-preview.png` shows both themes at 252, 90 and 28 pixels high. These sizes
  were visually checked on 2026-09-28.

The geometry is the site mark's, on its 32-unit grid. Each shard is drawn whole in its
lit-face grey, then its solid face on top, so the ridge between the faces has no
anti-aliasing seam. The solid face is the one left of the ridge.

The lit faces are solid greys, vorpal's blade-face colors: `#8e9399` in the light variant and
`#7c8794` in the dark one. On its intended background each looks almost the same as ink at
55% opacity, but it stays visible on the other background too. That matters because GitHub
picks the variant with the browser's `prefers-color-scheme`, not its own theme setting. A
reader whose GitHub is dark and whose system is light gets the light variant on a dark page,
where ink alone disappears. This was checked on 2026-09-28 in all four combinations of
variant and background.

One vector mask cuts the three glints, 0.45 units wide with round caps, and uses black and
white whatever the artwork's theme color. The viewBox leaves one unit around the shards.

The background and the cuts are transparent. The assets contain no embedded bitmaps,
filters, fonts, scripts or external resources.

Reproduce the transparent export from the repository root with:

```sh
rsvg-convert --width 1008 --height 1080 \
  --output docs/assets/brand/shards-fleet-transparent.png \
  docs/assets/brand/shards-fleet.svg
```

The root README selects the theme with a `<picture>` element and links to the preview.
