// Renders Visor's app icon into an .iconset directory:
//   swift make-icon.swift <out.iconset>
// A dark rounded square with a gradient V (the visor) and a coral record dot.
import AppKit

let out = URL(fileURLWithPath: CommandLine.arguments[1])
try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

func gradient(_ ctx: CGContext, _ colors: [CGColor], from: CGPoint, to: CGPoint) {
    let g = CGGradient(colorsSpace: srgb, colors: colors as CFArray, locations: nil)!
    ctx.drawLinearGradient(g, start: from, end: to, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
}

func render(_ px: Int) -> Data {
    let s = CGFloat(px)
    let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0, space: srgb,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

    // Body on Apple's icon grid: 824/1024 with ~22.5% corner radius.
    let inset = s * 100 / 1024
    let body = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let w = body.width
    let path = CGPath(roundedRect: body, cornerWidth: w * 0.225, cornerHeight: w * 0.225, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.012), blur: s * 0.03, color: rgb(0, 0.35))
    ctx.addPath(path)
    ctx.setFillColor(rgb(0x0C0D11))
    ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    gradient(ctx, [rgb(0x262A35), rgb(0x0C0D11)],
             from: CGPoint(x: body.minX, y: body.maxY), to: CGPoint(x: body.maxX, y: body.minY))
    ctx.restoreGState()

    // The V: one round-capped stroke filled with a warm-to-violet gradient.
    let top = body.minY + w * 0.70, bottom = body.minY + w * 0.22
    let left = body.midX - w * 0.27, right = body.midX + w * 0.27
    ctx.saveGState()
    ctx.setLineWidth(w * 0.15)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.move(to: CGPoint(x: left, y: top))
    ctx.addLine(to: CGPoint(x: body.midX, y: bottom))
    ctx.addLine(to: CGPoint(x: right, y: top))
    ctx.replacePathWithStrokedPath()
    ctx.clip()
    gradient(ctx, [rgb(0xFF9A62), rgb(0xFF4D6D), rgb(0x9B5CFF)],
             from: CGPoint(x: left, y: top), to: CGPoint(x: right, y: bottom))
    ctx.restoreGState()

    // Record dot above the V, with a soft glow.
    let r = w * 0.085
    let c = CGPoint(x: body.midX, y: top + w * 0.035)
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: r * 1.2, color: rgb(0xFF4D5E, 0.8))
    ctx.setFillColor(rgb(0xFF4D5E))
    ctx.fillEllipse(in: CGRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r))
    ctx.restoreGState()

    return NSBitmapImageRep(cgImage: ctx.makeImage()!).representation(using: .png, properties: [:])!
}

for size in [16, 32, 128, 256, 512] {
    try! render(size).write(to: out.appendingPathComponent("icon_\(size)x\(size).png"))
    try! render(size * 2).write(to: out.appendingPathComponent("icon_\(size)x\(size)@2x.png"))
}
