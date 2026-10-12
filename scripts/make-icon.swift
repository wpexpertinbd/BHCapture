// BHCapture icon generator — the single source of truth for every icon in the app.
//
//   swiftc -O scripts/make-icon.swift -o /tmp/make-icon && /tmp/make-icon
//
// Draws the icon from vectors at EVERY size (never scales a bitmap down), so 16 px is as crisp as
// 1024 px. Emits: icon.icns, icon.ico, the PNG set Tauri expects, the menu-bar template glyph, a
// 1024 master for READMEs, and a preview sheet for eyeballing.
//
// Family look (BHTerminal / BHDisplay / BHNotepad / BHUninstaller / BHServe): a blue gradient
// squircle, one white object that says what the app does, pale-blue detail, one accent colour.
import AppKit
import CoreGraphics

let srgb = CGColorSpace(name: CGColorSpace.sRGB)!
func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: srgb, components: [
        CGFloat((hex >> 16) & 255) / 255, CGFloat((hex >> 8) & 255) / 255, CGFloat(hex & 255) / 255, a])!
}
func gradient(_ stops: [(CGFloat, CGColor)]) -> CGGradient {
    CGGradient(colorsSpace: srgb, colors: stops.map { $0.1 } as CFArray, locations: stops.map { $0.0 })!
}
func context(_ w: Int, _ h: Int) -> CGContext {
    let c = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: srgb,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    c.interpolationQuality = .high
    return c
}

/// Apple's icon shape is a continuous-curvature squircle, not a rounded rectangle; a superellipse
/// of exponent ~5 is the standard close approximation.
func squircle(_ r: CGRect, n: Double = 5.0) -> CGPath {
    let p = CGMutablePath()
    let a = Double(r.width) / 2, b = Double(r.height) / 2
    let cx = Double(r.midX), cy = Double(r.midY), steps = 720
    for i in 0...steps {
        let t = 2 * Double.pi * Double(i) / Double(steps)
        let ct = cos(t), st = sin(t)
        let x = cx + a * (ct < 0 ? -1 : 1) * pow(abs(ct), 2 / n)
        let y = cy + b * (st < 0 ? -1 : 1) * pow(abs(st), 2 / n)
        i == 0 ? p.move(to: CGPoint(x: x, y: y)) : p.addLine(to: CGPoint(x: x, y: y))
    }
    p.closeSubpath()
    return p
}

/// One viewfinder corner: an L with a rounded elbow. (dx,dy) = which corner (+1/-1 each axis).
func corner(_ p: CGMutablePath, x: CGFloat, y: CGFloat, dx: CGFloat, dy: CGFloat, arm: CGFloat, radius: CGFloat) {
    p.move(to: CGPoint(x: x, y: y + dy * arm))
    p.addArc(tangent1End: CGPoint(x: x, y: y), tangent2End: CGPoint(x: x + dx * arm, y: y), radius: radius)
    p.addLine(to: CGPoint(x: x + dx * arm, y: y))
}

// MARK: - App icon (designed on a 1024 grid, y up)

func appIcon(_ n: Int) -> CGImage {
    let c = context(n, n)
    let s = CGFloat(n) / 1024
    // Detail that cannot survive at tiny sizes is dropped rather than smeared.
    let detail = n >= 64

    c.scaleBy(x: s, y: s)
    let tile = CGRect(x: 100, y: 100, width: 824, height: 824)
    let path = squircle(tile)

    // Drop shadow under the tile. Shadow params are in device units, so scale them by hand.
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -14 * s), blur: 30 * s, color: rgb(0x0A1F5C, 0.45))
    c.addPath(path); c.setFillColor(rgb(0x1B55D4)); c.fillPath()
    c.restoreGState()

    // Body gradient + top sheen, clipped to the squircle.
    c.saveGState()
    c.addPath(path); c.clip()
    c.drawLinearGradient(gradient([(0, rgb(0x56AAFF)), (0.55, rgb(0x2D7BF0)), (1, rgb(0x1648C8))]),
                         start: CGPoint(x: 0, y: 924), end: CGPoint(x: 0, y: 100), options: [])
    c.drawLinearGradient(gradient([(0, rgb(0xFFFFFF, 0.24)), (1, rgb(0xFFFFFF, 0))]),
                         start: CGPoint(x: 0, y: 924), end: CGPoint(x: 0, y: 600), options: [])
    c.addPath(path); c.setStrokeColor(rgb(0xFFFFFF, 0.30)); c.setLineWidth(5); c.strokePath()
    c.restoreGState()

    // Card: the "window" being captured.
    let card = CGRect(x: 330, y: 372, width: 364, height: 280)
    let cardPath = CGPath(roundedRect: card, cornerWidth: 38, cornerHeight: 38, transform: nil)
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -10 * s), blur: 24 * s, color: rgb(0x07205F, 0.35))
    c.addPath(cardPath); c.setFillColor(rgb(0xFFFFFF)); c.fillPath()
    c.restoreGState()

    c.saveGState()
    c.addPath(cardPath); c.clip()
    c.drawLinearGradient(gradient([(0, rgb(0xFFFFFF)), (1, rgb(0xE0EAFF))]),
                         start: CGPoint(x: 0, y: card.maxY), end: CGPoint(x: 0, y: card.minY), options: [])
    if detail {
        // Title bar, the record dot, and two lines of "content".
        c.setFillColor(rgb(0xCBDAFB))
        c.fill(CGRect(x: card.minX, y: card.maxY - 74, width: card.width, height: 74))
        c.setFillColor(rgb(0xFF5A4B))
        c.fillEllipse(in: CGRect(x: card.minX + 30, y: card.maxY - 37 - 19, width: 38, height: 38))
        c.setFillColor(rgb(0xA8BFF2))
        for (w, y) in [(250.0, 508.0), (170.0, 450.0)] {
            c.addPath(CGPath(roundedRect: CGRect(x: card.minX + 36, y: y, width: w, height: 28),
                             cornerWidth: 14, cornerHeight: 14, transform: nil))
            c.fillPath()
        }
    } else {
        c.setFillColor(rgb(0xFF5A4B))
        c.fillEllipse(in: CGRect(x: card.midX - 40, y: card.midY - 40, width: 80, height: 80))
    }
    c.restoreGState()

    // Viewfinder corners: what makes it a CAPTURE tool.
    let lo: CGFloat = 262, hi: CGFloat = 762
    let arm: CGFloat = 150, radius: CGFloat = 46
    let brackets = CGMutablePath()
    corner(brackets, x: lo, y: hi, dx: 1, dy: -1, arm: arm, radius: radius)
    corner(brackets, x: hi, y: hi, dx: -1, dy: -1, arm: arm, radius: radius)
    corner(brackets, x: lo, y: lo, dx: 1, dy: 1, arm: arm, radius: radius)
    corner(brackets, x: hi, y: lo, dx: -1, dy: 1, arm: arm, radius: radius)
    c.saveGState()
    c.setShadow(offset: CGSize(width: 0, height: -7 * s), blur: 14 * s, color: rgb(0x07205F, 0.35))
    c.addPath(brackets)
    c.setStrokeColor(rgb(0xFFFFFF)); c.setLineWidth(54); c.setLineCap(.round); c.setLineJoin(.round)
    c.strokePath()
    c.restoreGState()

    return c.makeImage()!
}

// MARK: - Menu-bar glyph (template: only the alpha channel matters, macOS recolours it)

func trayGlyph(_ n: Int) -> CGImage {
    let c = context(n, n)
    let s = CGFloat(n) / 36
    c.scaleBy(x: s, y: s)
    let lo: CGFloat = 4.5, hi: CGFloat = 31.5
    let p = CGMutablePath()
    corner(p, x: lo, y: hi, dx: 1, dy: -1, arm: 11, radius: 3.4)
    corner(p, x: hi, y: hi, dx: -1, dy: -1, arm: 11, radius: 3.4)
    corner(p, x: lo, y: lo, dx: 1, dy: 1, arm: 11, radius: 3.4)
    corner(p, x: hi, y: lo, dx: -1, dy: 1, arm: 11, radius: 3.4)
    c.addPath(p)
    c.setStrokeColor(rgb(0x000000)); c.setLineWidth(3.4); c.setLineCap(.round); c.setLineJoin(.round)
    c.strokePath()
    c.setFillColor(rgb(0x000000))
    c.fillEllipse(in: CGRect(x: 18 - 4.6, y: 18 - 4.6, width: 9.2, height: 9.2)) // the record dot
    return c.makeImage()!
}

// MARK: - Output

func png(_ img: CGImage) -> Data {
    NSBitmapImageRep(cgImage: img).representation(using: .png, properties: [:])!
}
func write(_ data: Data, _ path: String) { try! data.write(to: URL(fileURLWithPath: path)) }

/// Windows .ico: a directory of PNG-compressed images (supported since Vista).
func ico(_ sizes: [Int]) -> Data {
    let images = sizes.map { png(appIcon($0)) }
    var d = Data()
    func le16(_ v: Int) { d.append(contentsOf: [UInt8(v & 255), UInt8((v >> 8) & 255)]) }
    func le32(_ v: Int) { d.append(contentsOf: [UInt8(v & 255), UInt8((v >> 8) & 255), UInt8((v >> 16) & 255), UInt8((v >> 24) & 255)]) }
    le16(0); le16(1); le16(sizes.count)
    var offset = 6 + 16 * sizes.count
    for (i, sz) in sizes.enumerated() {
        d.append(UInt8(sz >= 256 ? 0 : sz)); d.append(UInt8(sz >= 256 ? 0 : sz))
        d.append(0); d.append(0); le16(1); le16(32)
        le32(images[i].count); le32(offset)
        offset += images[i].count
    }
    for i in images { d.append(i) }
    return d
}

let args = CommandLine.arguments
let root = args.count > 1 ? args[1] : FileManager.default.currentDirectoryPath
let icons = root + "/app/src-tauri/icons"
let assets = root + "/assets"
let fm = FileManager.default
try? fm.createDirectory(atPath: icons, withIntermediateDirectories: true)
try? fm.createDirectory(atPath: assets, withIntermediateDirectories: true)

// iconset -> icns
let set = NSTemporaryDirectory() + "BHCapture.iconset"
try? fm.removeItem(atPath: set)
try! fm.createDirectory(atPath: set, withIntermediateDirectories: true)
for (name, px) in [("icon_16x16", 16), ("icon_16x16@2x", 32), ("icon_32x32", 32), ("icon_32x32@2x", 64),
                   ("icon_128x128", 128), ("icon_128x128@2x", 256), ("icon_256x256", 256),
                   ("icon_256x256@2x", 512), ("icon_512x512", 512), ("icon_512x512@2x", 1024)] {
    write(png(appIcon(px)), "\(set)/\(name).png")
}
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", set, "-o", icons + "/icon.icns"]
try! p.run(); p.waitUntilExit()
precondition(p.terminationStatus == 0, "iconutil failed")

write(png(appIcon(32)), icons + "/32x32.png")
write(png(appIcon(128)), icons + "/128x128.png")
write(png(appIcon(256)), icons + "/128x128@2x.png")
write(png(appIcon(512)), icons + "/icon.png")
write(ico([16, 24, 32, 48, 64, 128, 256]), icons + "/icon.ico")
write(png(trayGlyph(36)), icons + "/tray.png")
write(png(appIcon(1024)), assets + "/BHCapture-1024.png")
print("wrote icns, ico, png set, tray glyph, README master")

// MARK: - Preview sheet (for eyeballing, not shipped)
if args.count > 2 {
    let W = 1500, H = 760
    let c = context(W, H)
    func band(_ r: CGRect, _ color: CGColor) { c.setFillColor(color); c.fill(r) }
    band(CGRect(x: 0, y: 0, width: W, height: H), rgb(0xF2F2F5))
    band(CGRect(x: 760, y: 0, width: 740, height: 380), rgb(0x1E1E22))
    func place(_ img: CGImage, _ x: Int, _ y: Int, _ size: Int) {
        c.draw(img, in: CGRect(x: x, y: y, width: size, height: size))
    }
    place(appIcon(1024), 20, 100, 640)
    var x = 700, y = 560
    for sz in [16, 32, 64, 128] { place(appIcon(sz), x, y - (sz > 64 ? 40 : 0), sz); x += sz + 50 }
    x = 700; y = 190
    for sz in [16, 32, 64, 128] { place(appIcon(sz), x, y, sz); x += sz + 50 }
    // Menu-bar glyph, 1x and 2x, on light and dark bars. A template is drawn in the bar colour.
    func tinted(_ n: Int, _ color: CGColor) -> CGImage {
        let t = context(n, n)
        t.draw(trayGlyph(n), in: CGRect(x: 0, y: 0, width: n, height: n))
        t.setBlendMode(.sourceIn); t.setFillColor(color); t.fill(CGRect(x: 0, y: 0, width: n, height: n))
        return t.makeImage()!
    }
    place(tinted(18, rgb(0x000000)), 700, 400, 18); place(tinted(36, rgb(0x000000)), 740, 396, 36)
    place(tinted(18, rgb(0xFFFFFF)), 1200, 100, 18); place(tinted(36, rgb(0xFFFFFF)), 1240, 96, 36)
    write(png(c.makeImage()!), args[2])
}
