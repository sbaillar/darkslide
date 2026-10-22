import CoreGraphics
import Foundation
import CoreImage
import CoreImage.CIFilterBuiltins

/// The pairing QR code as an image.
public enum QRCode {
    /// One software context for every code (a context is costly to make).
    private static let context = CIContext(options: [.useSoftwareRenderer: true])

    /// `text` as a QR code, each module `scale` pixels (medium error correction).
    public static func image(for text: String, scale: CGFloat = 8) -> CGImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage?.transformed(by: CGAffineTransform(scaleX: scale, y: scale)) else { return nil }
        return context.createCGImage(out, from: out.extent)
    }
}
