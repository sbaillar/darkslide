import CoreGraphics
import DarkslideCore
import SwiftUI

/// Pure path geometry for the canvas overlays (kept out of the view for tests).
public enum ViewerOverlayGeometry {
    /// Ellipse of a radial gradient in view points: `radius` is in units of the image's
    /// long edge on both axes (ds-core `MaskShape::Radial`, what ds-pipeline renders),
    /// `angleDegrees` turns the ellipse clockwise on screen.
    public static func radialPath(center: CGPoint, radius: CGSize, angleDegrees: Double, imageSize: CGSize) -> Path {
        let long = max(imageSize.width, imageSize.height)
        let rx = radius.width * long, ry = radius.height * long
        let t = CGAffineTransform(translationX: center.x, y: center.y)
            .rotated(by: angleDegrees * .pi / 180)
        return Path(ellipseIn: CGRect(x: -rx, y: -ry, width: 2 * rx, height: 2 * ry)).applying(t)
    }
}
