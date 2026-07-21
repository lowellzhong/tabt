---
name: app-icon
description: Regenerate the macOS app icon (bundle/AppIcon.icns) from tabt.png. Use when the logo artwork changes, when the icon looks wrong in the Dock or Finder (inset on a light plate, blurry, wrong size), or when auditing the icon's file size.
---

# Rebuilding TabT's app icon

```bash
.claude/skills/app-icon/make-icon.sh                    # tabt.png -> bundle/AppIcon.icns
.claude/skills/app-icon/make-icon.sh art.png out.icns   # explicit paths
make                                                    # re-bundle so TabT.app picks it up
```

The script is the source of truth. Everything below is *why* it does what it
does — the constraints were each found by trial, and every one of them has an
obvious-looking alternative that silently produces a broken icon.

## The one that actually matters: macOS 26 does not round your icon for you

Hand macOS a legacy `.icns` whose artwork is an **opaque square** and it will
shrink that square onto its own light rounded plate. The icon then reads as
floating inside a container rather than being the container. This is what
"图标没有填充满" looks like, and no amount of adjusting sizes or padding in the
source art fixes it.

The fix is that **the artwork must arrive already carrying the rounded-rect
alpha mask, bleeding to all four edges.** The script draws that mask (corner
radius = 0.2237 × side) and composites it in. Verified against a real app: the
result is pixel-comparable in shape and scale to Google Chrome's icon.

Do not copy the mask geometry from a stock app's `.icns` alpha. Chrome's
`app.icns` is inset to ~87% of its canvas, and reproducing that inset gets you
the plate treatment — because Chrome's *real* Tahoe icon ships in `Assets.car`
(Icon Composer), and the icns is vestigial. Full-bleed mask, always.

## Sizes

Five entries, one per distinct pixel size: **16, 32, 128, 256, 512**. No `@2x`
aliases. AppKit selects a representation by its pixel dimensions, not by the
slot's name, so `icon_256x256@2x.png` is just a byte-identical copy of the 512
— that duplication is what made the original icns 2 MB.

A 1024 entry is emitted only when the source is genuinely ≥1024. Never upscale
to fake it.

Source must be square and ≥128px. Below 512 the script warns, because the 512
slot is then upscaled. For **integer** upscale factors it uses nearest-neighbour
(`-filter Point`), which for this pixel-art logo adds no blur and no ringing and
round-trips bit-exactly; Lanczos would soften every hard pixel edge.

## Quantization

Entries are quantized to a 256-colour palette. On the textured artwork this cut
the icns ~72% (457 KB → 130 KB) at ~42 dB PSNR, i.e. visually lossless.

Quantize the RGB **before** attaching the alpha. Running `-colors` on an RGBA
image also quantizes the alpha channel and visibly jags the rounded corners.

## Verification

The script unpacks the icns it just wrote and asserts the 512 entry has **opaque
edge midpoints** (art bleeds to the edge) and **transparent corners** (the mask
survived). Those two probes are exactly the difference between a filled icon and
the plate treatment, so a green run means the icon is right.

To see what macOS will actually draw — the composited icon, plate and all —
don't screenshot the Dock (it's cached per bundle id, and the Dock may be
auto-hidden). Ask `NSWorkspace` directly:

```swift
// swiftc -O -o icondump icondump.swift && ./icondump <outdir> <path.app>
let icon = NSWorkspace.shared.icon(forFile: path)   // what Finder & Dock draw
```

## Traps

- `magick compare` exits **1** whenever the images differ at all, which quantized
  ones always do. Under `set -e` that kills the script mid-verify while leaving a
  correct-looking output file behind. Swallow the status; keep the metric.
- The Dock/Finder icon cache is keyed by **bundle id**, not by file contents.
  Two bundles with identical artwork render differently if one was launched
  earlier under a stale icon. Test icon changes in a throwaway bundle with a
  fresh `CFBundleIdentifier`, not by relaunching `TabT.app`.
- `make` does `rm -rf TabT.app` and recreates it. Don't stash anything in there.
- `bundle/AppIcon.icns` is committed, so the icon rebuild is a manual step. If
  you change `tabt.png`, run this skill or the bundle keeps the old icon.
