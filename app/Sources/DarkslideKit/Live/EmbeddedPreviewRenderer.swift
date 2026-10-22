import CoreGraphics
import Foundation

/// Live-mode `Renderer` until ds-pipeline is exported: draws the photo's
/// embedded JPEG preview. Edits are **not** applied, regions and float frames
/// are not supported (`RendererCapabilities`), and the viewer says so.
///
/// Previews come from the shared provider (one decode per file; a pane-sized
/// request at or above the preview's size gets the same image the viewer's
/// instant preview uses) and convert through `PreviewFrames`, so the 8-bit
/// frame of that image is shared too.
public struct EmbeddedPreviewRenderer: Renderer {
    let previews: any PreviewProvider
    let frames: PreviewFrames

    public init(previews: any PreviewProvider, frames: PreviewFrames = .shared) {
        self.previews = previews
        self.frames = frames
    }

    public func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        guard let url = photo.url else { throw MockError.notFound("file for \(photo.filename)") }
        // Whole-image only: a region/scale request still gets the whole preview (callers check capabilities).
        let edge = request.scale == nil ? max(0, request.maxPixelSize) : 0
        let img = try await previews.preview(for: url, maxPixelSize: edge)
        try Task.checkCancellation()
        guard let f = frames.frame(for: img) else { throw MockError.invalid("could not draw preview") }
        return f
    }

    /// Draw a CGImage into a packed 8-bit sRGB RGBA frame.
    static func frame(from img: CGImage) throws -> RenderedFrame {
        guard let f = PreviewFrames.convert(img) else { throw MockError.invalid("could not draw preview") }
        return f
    }
}
