import Foundation

/// Slider specs for presence, layer tools, brushes and spots.
public enum LayerToolSpecs {
    public static let vibrance = SliderSpec("Vibrance", -100...100, help: "Boost muted colours first, protecting skin.")
    public static let texture = SliderSpec("Texture", -100...100, help: "Medium-sized detail: + crisper, – smoother.")
    public static let dehaze = SliderSpec("Dehaze", -100...100, help: "Remove (+) or add (–) atmospheric haze.")
    public static let temperatureShift = SliderSpec("Temp", -3000...3000, step: 10, format: .signedInteger,
                                                    help: "Warm (+) or cool (–) this layer, in Kelvin.")
    public static let tintShift = SliderSpec("Tint", -150...150, help: "Green (–) to magenta (+) for this layer.")
    public static let sharpening = SliderSpec("Sharpen", -100...100, help: "Sharpen (+) or soften (–) under the mask.")
    public static let brushSize = SliderSpec("Size", 0.005...0.5, default: 0.05, step: 0.005, format: .percent,
                                             help: "Brush diameter, as a share of the long edge ([ and ]).")
    public static let brushHardness = SliderSpec("Hardness", 0...1, default: 0.5, step: 0.01, format: .percent,
                                                 help: "Hard (100 %) or soft brush edge.")
    public static let brushFlow = SliderSpec("Flow", 0.05...1, default: 1, step: 0.01, format: .percent,
                                             help: "How much each pass of the brush adds.")
    /// Up to lt-core `MAX_PLANE_FEATHER` (0.05 of the long edge): the pipeline caps it there
    /// so a feathered mask renders the same in every view and in exports.
    public static let maskFeather = SliderSpec("Feather", 0...0.05, step: 0.001, format: .percent,
                                               help: "Soften the whole mask's edge.")
    public static let spotSize = SliderSpec("Size", 0.005...0.2, default: 0.02, step: 0.005, format: .percent,
                                            help: "Spot radius, as a share of the long edge.")
    public static let lumaLow = SliderSpec("Darkest", 0...1, step: 0.01, format: .percent, help: "Tones below this are left out.")
    public static let lumaHigh = SliderSpec("Brightest", 0...1, default: 1, step: 0.01, format: .percent,
                                            help: "Tones above this are left out.")
    public static let lumaFalloff = SliderSpec("Falloff", 0...0.5, default: 0.1, step: 0.01, format: .percent,
                                               help: "How softly the range fades at both ends.")
}
