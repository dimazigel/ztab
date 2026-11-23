// Draws ZTab's app icon (1024x1024 PNG): a macOS-style rounded square with a row of window
// cards, the middle one selected — the switcher itself, in miniature.
// Usage: swift scripts/make-icon.swift <out.png>   (scripts/make-icon.sh builds the .icns)
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size), bitsPerSample: 8,
    samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
    bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

// Body: Big Sur grid — 824pt square centred, ~185pt corner radius, soft drop shadow.
let body = NSRect(x: 100, y: 100, width: 824, height: 824)
let bodyPath = NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185)
ctx.saveGState()
let shadow = NSShadow()
shadow.shadowColor = NSColor.black.withAlphaComponent(0.35)
shadow.shadowBlurRadius = 28
shadow.shadowOffset = NSSize(width: 0, height: -12)
shadow.set()
rgb(0x1B2340).setFill()
bodyPath.fill()
ctx.restoreGState()

// Background gradient (deep navy -> indigo), plus a faint top sheen.
NSGradient(colors: [rgb(0x2A3F8F), rgb(0x161D3A)])!.draw(in: bodyPath, angle: -90)
ctx.saveGState()
bodyPath.addClip()
NSGradient(colors: [NSColor.white.withAlphaComponent(0.10), NSColor.white.withAlphaComponent(0)])!
    .draw(in: NSRect(x: body.minX, y: body.midY, width: body.width, height: body.height / 2), angle: -90)
ctx.restoreGState()

// A window card: rounded rect, title bar with traffic lights, a few content lines.
func card(_ r: NSRect, fill: NSColor, bar: NSColor, line: NSColor, alpha: CGFloat) {
    let radius: CGFloat = r.width * 0.09
    let p = NSBezierPath(roundedRect: r, xRadius: radius, yRadius: radius)
    ctx.saveGState()
    let s = NSShadow()
    s.shadowColor = NSColor.black.withAlphaComponent(0.35 * alpha)
    s.shadowBlurRadius = 18
    s.shadowOffset = NSSize(width: 0, height: -8)
    s.set()
    fill.withAlphaComponent(alpha).setFill()
    p.fill()
    ctx.restoreGState()

    ctx.saveGState()
    p.addClip()
    let barH = r.height * 0.17
    bar.withAlphaComponent(alpha).setFill()
    NSRect(x: r.minX, y: r.maxY - barH, width: r.width, height: barH).fill()
    let d = barH * 0.36
    for (i, c) in [rgb(0xFF5F57), rgb(0xFEBC2E), rgb(0x28C840)].enumerated() {
        c.withAlphaComponent(alpha).setFill()
        NSBezierPath(ovalIn: NSRect(x: r.minX + barH * 0.42 + CGFloat(i) * d * 1.55,
                                    y: r.maxY - barH / 2 - d / 2, width: d, height: d)).fill()
    }
    line.withAlphaComponent(alpha).setFill()
    let lw = [0.72, 0.55, 0.64, 0.40]
    for (i, f) in lw.enumerated() {
        let lh = r.height * 0.07
        let y = r.maxY - barH - r.height * 0.13 - CGFloat(i) * lh * 1.9
        NSBezierPath(roundedRect: NSRect(x: r.minX + r.width * 0.12, y: y, width: r.width * CGFloat(f), height: lh),
                     xRadius: lh / 2, yRadius: lh / 2).fill()
    }
    ctx.restoreGState()
}

// Three cards in a row (the switcher strip); outer ones dimmed and smaller.
let cw: CGFloat = 236, ch: CGFloat = 188
let cy = body.midY + 70
let sideW = cw * 0.76, sideH = ch * 0.76
let sideGap: CGFloat = 22 + 34  // selection ring overhang + gap
card(NSRect(x: body.midX - cw / 2 - sideGap - sideW, y: cy - sideH / 2, width: sideW, height: sideH),
     fill: rgb(0xC9D3F2), bar: rgb(0x9DABD9), line: rgb(0x8C9AC9), alpha: 0.55)
card(NSRect(x: body.midX + cw / 2 + sideGap, y: cy - sideH / 2, width: sideW, height: sideH),
     fill: rgb(0xC9D3F2), bar: rgb(0x9DABD9), line: rgb(0x8C9AC9), alpha: 0.55)

// Selected (middle) card: white, with an accent selection ring like the real switcher.
let sel = NSRect(x: body.midX - cw / 2, y: cy - ch / 2, width: cw, height: ch)
let ring = NSBezierPath(roundedRect: sel.insetBy(dx: -22, dy: -22), xRadius: 42, yRadius: 42)
rgb(0x5B8CFF, 0.28).setFill()
ring.fill()
ring.lineWidth = 12
rgb(0x6E9BFF).setStroke()
ring.stroke()
card(sel, fill: rgb(0xFFFFFF), bar: rgb(0xE3E8F7), line: rgb(0xB9C4E6), alpha: 1)

// "⌥ ⇥" key caps under the strip.
func keycap(_ text: String, _ center: NSPoint) {
    let w: CGFloat = 130, h: CGFloat = 104
    let r = NSRect(x: center.x - w / 2, y: center.y - h / 2, width: w, height: h)
    let p = NSBezierPath(roundedRect: r, xRadius: 24, yRadius: 24)
    rgb(0xFFFFFF, 0.14).setFill()
    p.fill()
    p.lineWidth = 4
    rgb(0xFFFFFF, 0.35).setStroke()
    p.stroke()
    let attrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 66, weight: .semibold),
        .foregroundColor: NSColor.white.withAlphaComponent(0.92),
    ]
    let s = NSAttributedString(string: text, attributes: attrs)
    let sz = s.size()
    s.draw(at: NSPoint(x: center.x - sz.width / 2, y: center.y - sz.height / 2 + 2))
}
keycap("⌥", NSPoint(x: body.midX - 82, y: body.minY + 215))
keycap("⇥", NSPoint(x: body.midX + 82, y: body.minY + 215))

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
