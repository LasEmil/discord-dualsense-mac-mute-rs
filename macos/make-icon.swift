#!/usr/bin/env swift
//
// Generates AppIcon.icns for DiscordMute: a Discord-blurple squircle with a
// white microphone glyph (the same mic motif the menu bar and panel use).
//
// Draws every icon size natively with AppKit — the mic itself is the SF Symbol
// `mic.fill`, so the glyph is professionally shaped rather than hand-traced.
//
// Run from the `macos/` directory:  swift make-icon.swift
// Produces AppIcon.iconset/ and AppIcon.icns beside this script.

import AppKit

// Apple's icon grid leaves transparent margin around the rounded rect and uses
// a continuous ("squircle") corner. These ratios approximate that shape closely
// enough to read as native at every size.
let squircleInset = 0.10      // transparent margin, fraction of the canvas
let cornerRatio = 0.2237      // corner radius as a fraction of the squircle side
let glyphRatio = 0.46         // mic height as a fraction of the canvas

// Blurple, lit from the top for a little depth. Discord's #5865F2 sits between.
let topColor = NSColor(srgbRed: 0x63 / 255, green: 0x74 / 255, blue: 0xF7 / 255, alpha: 1)
let bottomColor = NSColor(srgbRed: 0x3D / 255, green: 0x44 / 255, blue: 0xC4 / 255, alpha: 1)

/// A rounded rect with continuous-looking corners, inset from the full canvas.
func squirclePath(canvas: CGFloat) -> NSBezierPath {
    let inset = canvas * squircleInset
    let rect = NSRect(x: inset, y: inset, width: canvas - inset * 2, height: canvas - inset * 2)
    let radius = rect.width * cornerRatio
    return NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
}

/// The white mic glyph, tinted by compositing white over the symbol's shape.
func micGlyph(side: CGFloat) -> NSImage {
    let point = side * glyphRatio
    let config = NSImage.SymbolConfiguration(pointSize: point, weight: .semibold)
    guard
        let base = NSImage(systemSymbolName: "mic.fill", accessibilityDescription: "Microphone"),
        let symbol = base.withSymbolConfiguration(config)
    else {
        fatalError("mic.fill is unavailable on this system")
    }

    let tinted = NSImage(size: symbol.size)
    tinted.lockFocus()
    symbol.draw(at: .zero, from: .zero, operation: .sourceOver, fraction: 1)
    NSColor.white.set()
    NSRect(origin: .zero, size: symbol.size).fill(using: .sourceAtop)
    tinted.unlockFocus()
    return tinted
}

func renderIcon(pixels: Int) -> NSBitmapImageRep {
    let side = CGFloat(pixels)
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4,
        hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0
    )!
    rep.size = NSSize(width: side, height: side)

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

    let path = squirclePath(canvas: side)
    let gradient = NSGradient(starting: bottomColor, ending: topColor)!
    gradient.draw(in: path, angle: 90)

    // Center the mic on the squircle, nudged up a hair so it sits optically
    // centred rather than mathematically centred.
    let glyph = micGlyph(side: side)
    let origin = NSPoint(
        x: (side - glyph.size.width) / 2,
        y: (side - glyph.size.height) / 2 + side * 0.01
    )
    glyph.draw(at: origin, from: .zero, operation: .sourceOver, fraction: 1)

    NSGraphicsContext.restoreGraphicsState()
    return rep
}

func writePNG(_ rep: NSBitmapImageRep, to url: URL) {
    guard let data = rep.representation(using: .png, properties: [:]) else {
        fatalError("failed to encode \(url.lastPathComponent)")
    }
    try! data.write(to: url)
}

let here = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let iconset = here.appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

// (base point size, retina scale) → the iconset filenames macOS expects.
let variants: [(Int, Int)] = [
    (16, 1), (16, 2), (32, 1), (32, 2), (128, 1),
    (128, 2), (256, 1), (256, 2), (512, 1), (512, 2),
]

for (base, scale) in variants {
    let pixels = base * scale
    let rep = renderIcon(pixels: pixels)
    let suffix = scale == 1 ? "" : "@2x"
    let name = "icon_\(base)x\(base)\(suffix).png"
    writePNG(rep, to: iconset.appendingPathComponent(name))
}

// A standalone preview at 512 for eyeballing without opening the bundle.
writePNG(renderIcon(pixels: 512), to: here.appendingPathComponent("AppIcon-preview.png"))

print("Wrote \(iconset.path) and AppIcon-preview.png")
