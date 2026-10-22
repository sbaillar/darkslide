import Foundation

/// The filmstrip's edge print: the line under each frame, like the numbers on a
/// negative's rebate ("1005 ▸ ★★★ EDITED").
public enum FilmEdge {
    /// The frame number: the last four digits of the file name's stem
    /// ("DSC01005.ARW" → "1005"); without digits, the stem's first four characters.
    public static func frameNumber(_ filename: String) -> String {
        let stem = (filename as NSString).deletingPathExtension
        let digits = stem.reversed().prefix { $0.isNumber }.reversed()
        if !digits.isEmpty { return String(digits.suffix(4)) }
        return String(stem.prefix(4)).uppercased()
    }

    /// The whole line: frame number, stars, flag and "EDITED", unset parts left out.
    public static func caption(_ photo: Photo) -> String {
        var parts = ["\(frameNumber(photo.filename)) ▸"]
        if photo.rating > 0 { parts.append(String(repeating: "★", count: min(photo.rating, 5))) }
        switch photo.pick {
        case .picked: parts.append("PICK")
        case .rejected: parts.append("REJECT")
        case .none: break
        }
        if photo.isEdited { parts.append("EDITED") }
        return parts.joined(separator: " ")
    }

    /// Sprocket holes across a strip `width` wide: the x of each hole's left edge,
    /// `pitch` apart and centred so both ends are trimmed alike.
    public static func sprocketXs(width: Double, hole: Double = 6, pitch: Double = 14) -> [Double] {
        guard width >= hole, pitch > 0 else { return [] }
        let count = Int((width - hole) / pitch) + 1
        let start = (width - (Double(count - 1) * pitch + hole)) / 2
        return (0..<count).map { start + Double($0) * pitch }
    }
}

/// The tool sliders' ruler: fine ticks, a taller one every `majorEvery`.
public enum SliderRuler {
    public struct Tick: Equatable, Sendable {
        public let fraction: Double
        public let isMajor: Bool
    }

    /// Ticks for a track `width` points wide: about one every `spacing` points, always a
    /// multiple of `majorEvery` divisions so both ends and the middle land on major ticks.
    public static func ticks(width: Double, spacing: Double = 5, majorEvery: Int = 5) -> [Tick] {
        guard width > 0, spacing > 0, majorEvery > 0 else { return [] }
        let groups = max(2, Int((width / spacing / Double(majorEvery)).rounded()) / 2 * 2)
        let divisions = groups * majorEvery
        return (0...divisions).map { Tick(fraction: Double($0) / Double(divisions), isMajor: $0 % majorEvery == 0) }
    }
}
