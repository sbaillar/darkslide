import CoreGraphics
import CoreImage
import Foundation

/// The navigator's small SDR image of a frame, scaled and converted on the GPU with
/// Core Image (gpu-audit #1). A 16F linear-P3 frame used to go through
/// `ColorMath.sdrFrame` on the main thread: ~96 ms for a 2560 px frame, on every
/// navigation while zoomed with the navigator on an EDR display.
enum NavigatorThumbnail {
    /// Long edge of the navigator image, in pixels (the panel is ~180 pt at 2×).
    static let maxEdge = 360

    private static let context = CIContext(options: [
        .workingColorSpace: CGColorSpace(name: CGColorSpace.extendedLinearSRGB)!,
        .cacheIntermediates: false,
    ])

    /// An 8-bit sRGB image of `frame`, at most `maxEdge` on its long side. `nil` when the
    /// frame can't be read.
    static func make(_ frame: RenderedFrame, maxEdge: Int = maxEdge) -> CGImage? {
        guard frame.width > 0, frame.height > 0 else { return nil }
        let (format, space): (CIFormat, CGColorSpace) = switch frame.format {
        case .rgba16fLinearP3: (.RGBAh, CGColorSpace(name: CGColorSpace.extendedLinearDisplayP3)!)
        case .rgba16fLinearRec2020: (.RGBAh, CGColorSpace(name: CGColorSpace.extendedLinearITUR_2020)!)
        case .rgba8DisplayP3: (.RGBA8, CGColorSpace(name: CGColorSpace.displayP3)!)
        case .rgba8Srgb: (.RGBA8, CGColorSpace(name: CGColorSpace.sRGB)!)
        }
        let image = CIImage(bitmapData: frame.rgba, bytesPerRow: frame.bytesPerRow,
                            size: CGSize(width: frame.width, height: frame.height), format: format, colorSpace: space)
        let scale = min(1, Double(maxEdge) / Double(max(frame.width, frame.height)))
        let scaled = scale < 1
            ? image.applyingFilter("CILanczosScaleTransform", parameters: [kCIInputScaleKey: scale, kCIInputAspectRatioKey: 1])
            : image
        return context.createCGImage(scaled, from: scaled.extent.integral, format: .RGBA8,
                                     colorSpace: CGColorSpace(name: CGColorSpace.sRGB)!)
    }
}
