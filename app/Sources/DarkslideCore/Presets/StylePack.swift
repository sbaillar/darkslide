import Foundation

/// The built-in styles: the active plug-ins' styles (Darkslide's own 20 in five sections
/// are the bundled `darkslide.looks` plug-in). Tone groups are relative (they add to the
/// photo's own exposure/HDR), everything else touches only the fields the style sets, so
/// a style never undoes the user's correction (M9). Ids are stable.
extension Preset {
    /// Sections in order of first appearance.
    public static var builtInCategories: [String] { PluginContent.current.styleSections }

    public static var builtIns: [Preset] { PluginContent.current.styles }
}
