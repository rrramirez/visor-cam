// Renders Visor's app icon into an .iconset directory:
//   swift make-icon.swift <out.iconset>
// A dark rounded square with a lens ring and a red record dot.
import AppKit

let out = URL(fileURLWithPath: CommandLine.arguments[1])
try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)

func render(_ px: Int) -> Data {
    let s = CGFloat(px)
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let ctx = NSGraphicsContext.current!.cgContext

    // Body on Apple's icon grid: 824/1024 with ~22% corner radius.
    let inset = s * 100 / 1024
    let body = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let path = CGPath(roundedRect: body, cornerWidth: body.width * 0.225, cornerHeight: body.width * 0.225, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.01), blur: s * 0.03, color: NSColor.black.withAlphaComponent(0.35).cgColor)
    ctx.addPath(path)
    ctx.setFillColor(NSColor(srgbRed: 0.11, green: 0.12, blue: 0.14, alpha: 1).cgColor)
    ctx.fillPath()
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
                              colors: [NSColor(srgbRed: 0.20, green: 0.21, blue: 0.25, alpha: 1).cgColor,
                                       NSColor(srgbRed: 0.08, green: 0.085, blue: 0.10, alpha: 1).cgColor] as CFArray,
                              locations: [0, 1])!
    ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: body.maxY), end: CGPoint(x: 0, y: body.minY), options: [])
    ctx.restoreGState()

    let c = CGPoint(x: s / 2, y: s / 2)
    // Lens ring.
    let ringR = body.width * 0.30
    ctx.setStrokeColor(NSColor(srgbRed: 0.93, green: 0.94, blue: 0.96, alpha: 0.92).cgColor)
    ctx.setLineWidth(body.width * 0.055)
    ctx.strokeEllipse(in: CGRect(x: c.x - ringR, y: c.y - ringR, width: 2 * ringR, height: 2 * ringR))
    // Record dot.
    let dotR = body.width * 0.17
    ctx.setFillColor(NSColor(srgbRed: 1.0, green: 0.30, blue: 0.37, alpha: 1).cgColor)
    ctx.fillEllipse(in: CGRect(x: c.x - dotR, y: c.y - dotR, width: 2 * dotR, height: 2 * dotR))

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for size in [16, 32, 128, 256, 512] {
    try! render(size).write(to: out.appendingPathComponent("icon_\(size)x\(size).png"))
    try! render(size * 2).write(to: out.appendingPathComponent("icon_\(size)x\(size)@2x.png"))
}
