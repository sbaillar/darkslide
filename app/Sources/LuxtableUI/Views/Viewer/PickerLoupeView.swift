import LuxtableCore
import SwiftUI

/// The eyedroppers' magnifier: a circle up and to the right of the cursor showing the
/// full-resolution develop around the pixel under it, nearest-neighbour, with the
/// sample area outlined, and the readout below. Only this view observes the loupe, so
/// moving the cursor redraws nothing else.
struct PickerLoupeView: View {
    let loupe: PickerLoupe

    var body: some View {
        if loupe.shown, let cursor = loupe.cursor, let kind = loupe.kind {
            let d = PickerLoupe.diameter
            let rect = PickerLoupe.placement(cursor: cursor, bounds: loupe.bounds)
            VStack(spacing: 6) {
                magnifier(diameter: d)
                    .frame(width: d, height: d)
                LoupeReadoutView(readout: loupe.readout, kind: kind, sample: loupe.sample)
            }
            .frame(width: max(d, 188))
            .position(x: rect.midX, y: rect.minY + (d + 6 + PickerLoupe.readoutHeight) / 2)
            .allowsHitTesting(false)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(loupe.readout.map { PickerLoupe.spoken($0, kind: kind) } ?? "Magnifier")
            // Debounced: a new readout cancels the pending one, so VoiceOver speaks
            // where the cursor rests, not every pixel it crosses.
            .task(id: loupe.announcement) {
                try? await Task.sleep(for: .milliseconds(350))
                if !Task.isCancelled { Self.announce(loupe.announcement) }
            }
        }
    }

    @ViewBuilder private func magnifier(diameter d: CGFloat) -> some View {
        let mag = loupe.magnification
        ZStack {
            Circle().fill(Color.black)
            if let region = loupe.region, let px = loupe.pixel, let local = region.local(px) {
                LoupePixels(region: region, centre: local, magnification: mag)
            } else {
                ProgressView().controlSize(.small)
            }
            Reticle(side: Double(loupe.sample.pixels) * mag)
        }
        .clipShape(Circle())
        .overlay { Circle().strokeBorder(.white.opacity(0.9), lineWidth: 2) }
        .overlay { Circle().strokeBorder(.black.opacity(0.35), lineWidth: 0.5) }
        .shadow(color: .black.opacity(0.45), radius: 6, y: 2)
    }

    /// VoiceOver: say the settled readout.
    static func announce(_ text: String?) {
        if let text { Announcer.announce(text) }
    }
}

/// The region's pixels around `centre`, each a filled square (nearest-neighbour by
/// construction: image scaling can smooth them), with faint boundaries when large.
private struct LoupePixels: View {
    let region: PickerLoupe.Region
    let centre: (x: Int, y: Int)
    let magnification: Double

    var body: some View {
        Canvas(rendersAsynchronously: false) { ctx, size in
            let f = region.frame
            // Points per pixel of the region's frame (1:1 frames: the magnification).
            let ppp = magnification * region.rect.width * region.space.width / Double(max(f.width, 1))
            let reach = Int((size.width / 2 / ppp).rounded(.up)) + 1
            let space: Color.RGBColorSpace = f.format == .rgba8DisplayP3 ? .displayP3 : .sRGB
            f.withBytes { (b: UnsafeRawBufferPointer) in
                for y in max(centre.y - reach, 0)...min(centre.y + reach, f.height - 1) {
                    for x in max(centre.x - reach, 0)...min(centre.x + reach, f.width - 1) {
                        let i = y * f.bytesPerRow + x * 4
                        let cell = CGRect(x: size.width / 2 + (Double(x - centre.x) - 0.5) * ppp,
                                          y: size.height / 2 + (Double(y - centre.y) - 0.5) * ppp, width: ppp, height: ppp)
                        ctx.fill(Path(cell.insetBy(dx: -0.25, dy: -0.25)),
                                 with: .color(Color(space, red: Double(b[i]) / 255, green: Double(b[i + 1]) / 255,
                                                    blue: Double(b[i + 2]) / 255)))
                        if ppp >= 10 { ctx.stroke(Path(cell), with: .color(.black.opacity(0.16)), lineWidth: 0.5) }
                    }
                }
            }
        }
    }
}

/// The sample area outlined (light over dark, visible on any colour), with ticks.
private struct Reticle: View {
    let side: Double

    var body: some View {
        ZStack {
            Rectangle().strokeBorder(.black.opacity(0.8), lineWidth: 3).frame(width: side + 3, height: side + 3)
            Rectangle().strokeBorder(.white, lineWidth: 1).frame(width: side + 1, height: side + 1)
            ForEach(0..<4) { i in
                Capsule().fill(.white)
                    .frame(width: 1.5, height: 10)
                    .shadow(color: .black, radius: 0.5)
                    .offset(y: -(side / 2 + 10))
                    .rotationEffect(.degrees(Double(i) * 90))
            }
        }
    }
}

/// RGB, the white balance a click applies, how neutral it is, and a clipping warning.
private struct LoupeReadoutView: View {
    let readout: PickerLoupe.Readout?
    let kind: PickerLoupe.Kind
    let sample: PickerSample

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            if let r = readout {
                HStack(spacing: 8) {
                    Text("R \(r.rgb.x)").foregroundStyle(Color(red: 1, green: 0.55, blue: 0.55))
                    Text("G \(r.rgb.y)").foregroundStyle(Color(red: 0.55, green: 1, blue: 0.6))
                    Text("B \(r.rgb.z)").foregroundStyle(Color(red: 0.6, green: 0.75, blue: 1))
                    Spacer(minLength: 0)
                    Text(sample.label).foregroundStyle(Self.dim)
                }
                .font(.system(size: 11, weight: .semibold).monospacedDigit())
                if kind == .whiteBalance {
                    if let wb = r.whiteBalance {
                        Text("\(Formatters.kelvin(wb.temperature))  Tint \(Formatters.signed(wb.tint, decimals: 1))")
                            .font(.system(size: 11, weight: .semibold).monospacedDigit())
                    } else if let e = r.whiteBalanceError {
                        Text(e).foregroundStyle(DS.reject)
                    } else {
                        Text("Measuring…").foregroundStyle(Self.dim)
                    }
                } else {
                    Text(Self.hue(r.rgb)).monospacedDigit()
                }
                Text(r.neutrality).foregroundStyle(Self.dim)
                if r.clipped {
                    Label("Clipped: pick a non-blown neutral", systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(DS.reject)
                }
            } else {
                Text("Measuring…").foregroundStyle(Self.dim)
            }
        }
        .font(.system(size: 11))
        .lineLimit(1)
        .padding(.horizontal, 8).padding(.vertical, 5)
        .frame(width: 188, alignment: .leading)
        .background(.black.opacity(0.78), in: .rect(cornerRadius: 7))
        .foregroundStyle(.white)
        .environment(\.colorScheme, .dark)
    }

    /// Secondary text on the readout's fixed near-black plate (≥ 7:1).
    static let dim = Color.white.opacity(0.72)

    static func hue(_ rgb: SIMD3<Int>) -> String {
        let hs = ColorPick.hueSaturation(SIMD3(Double(rgb.x), Double(rgb.y), Double(rgb.z)) / 255)
        return "Hue \(Int(hs.hue.rounded()))°  Saturation \(Int((hs.saturation * 100).rounded()))%"
    }
}
