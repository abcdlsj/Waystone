import CoreGraphics

/// Where the remote picture sits on the touch surface, and how to map a touch
/// back to the remote screen.
///
/// The picture is aspect-fitted into the surface and then zoomed and panned
/// locally (pinch zoom never changes what the host sends). Every touch goes
/// through the inverse of that transform, so a tap lands on the pixel under
/// the finger whatever the zoom.
struct PictureGeometry: Equatable {
    /// Touch surface size, in points.
    var bounds: CGSize = .zero
    /// Decoded picture size, in pixels; only its aspect ratio matters.
    var videoSize: CGSize = .zero
    /// Local zoom, `1...maxScale`.
    private(set) var scale: CGFloat = 1
    /// Pan applied after zooming, in points.
    private(set) var offset: CGVector = .zero

    static let maxScale: CGFloat = 4

    /// The aspect-fitted picture before zoom, in surface coordinates.
    var contentRect: CGRect {
        guard bounds.width > 0, bounds.height > 0 else { return .zero }
        guard videoSize.width > 0, videoSize.height > 0 else { return CGRect(origin: .zero, size: bounds) }
        let fit = min(bounds.width / videoSize.width, bounds.height / videoSize.height)
        let size = CGSize(width: videoSize.width * fit, height: videoSize.height * fit)
        return CGRect(
            x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2,
            width: size.width, height: size.height)
    }

    /// The picture as drawn, after zoom and pan.
    var displayedRect: CGRect {
        let content = contentRect
        let center = CGPoint(x: bounds.width / 2, y: bounds.height / 2)
        return CGRect(
            x: center.x + (content.minX - center.x) * scale + offset.dx,
            y: center.y + (content.minY - center.y) * scale + offset.dy,
            width: content.width * scale, height: content.height * scale)
    }

    var isZoomed: Bool { scale > 1.001 }

    /// Remote position, `0...1` on both axes, for a point on the surface.
    func normalized(_ point: CGPoint) -> (x: Double, y: Double) {
        let rect = displayedRect
        guard rect.width > 0, rect.height > 0 else { return (0.5, 0.5) }
        let x = (point.x - rect.minX) / rect.width
        let y = (point.y - rect.minY) / rect.height
        return (Double(min(max(x, 0), 1)), Double(min(max(y, 0), 1)))
    }

    /// Surface position of a remote position; used to draw the local cursor.
    func surfacePoint(x: Double, y: Double) -> CGPoint {
        let rect = displayedRect
        return CGPoint(x: rect.minX + CGFloat(x) * rect.width, y: rect.minY + CGFloat(y) * rect.height)
    }

    /// Size of one remote "unit" (the whole picture) on screen, in points.
    var displayedSize: CGSize { displayedRect.size }

    /// Zooms by `factor` around `focus` and pans by `translation`, keeping the
    /// point under the fingers under the fingers.
    mutating func zoom(by factor: CGFloat, around focus: CGPoint, translation: CGVector) {
        let newScale = min(max(scale * factor, 1), Self.maxScale)
        let center = CGPoint(x: bounds.width / 2, y: bounds.height / 2)
        // Surface point (before zoom) under the focus.
        let anchorX = center.x + (focus.x - center.x - offset.dx) / scale
        let anchorY = center.y + (focus.y - center.y - offset.dy) / scale
        scale = newScale
        offset = CGVector(
            dx: focus.x + translation.dx - center.x - (anchorX - center.x) * scale,
            dy: focus.y + translation.dy - center.y - (anchorY - center.y) * scale)
        clampOffset()
    }

    mutating func resetZoom() {
        scale = 1
        offset = .zero
    }

    /// Keeps the zoomed picture from being dragged off the surface. The
    /// fitted picture is centered, so the slack on each axis is half of what
    /// the zoomed picture overflows the surface by.
    private mutating func clampOffset() {
        let content = contentRect
        let maxX = max(0, (content.width * scale - bounds.width) / 2)
        let maxY = max(0, (content.height * scale - bounds.height) / 2)
        offset.dx = min(max(offset.dx, -maxX), maxX)
        offset.dy = min(max(offset.dy, -maxY), maxY)
    }
}
