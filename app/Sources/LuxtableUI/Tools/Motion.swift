import LuxtableCore
import SwiftUI

/// Animation and transition choices that honour System Settings ▸ Accessibility ▸ Display ▸
/// Reduce motion: with it on, overlays fade instead of sliding or scaling, and state changes
/// don't animate.
enum Motion {
    static func animation(_ animation: Animation, reduce: Bool) -> Animation? {
        reduce ? nil : animation
    }

    static func transition(_ transition: AnyTransition, reduce: Bool) -> AnyTransition {
        reduce ? .opacity : transition
    }
}
