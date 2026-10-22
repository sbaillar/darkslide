import Foundation
import Observation

/// Keyboard, scroll-wheel, and drag input for tool sliders (pure; the views call these).
public enum SliderInput {
    public enum Direction: Sendable { case up, down }

    /// Arrow-key nudge: one step; ⇧ = 10 steps; ⌥ = a tenth of a step (at least the spec's precision).
    public static func nudge(_ value: Double, _ direction: Direction, spec: SliderSpec, shift: Bool = false,
                             option: Bool = false) -> Double {
        let sign: Double = direction == .up ? 1 : -1
        let amount = shift ? spec.step * 10 : option ? spec.step / ScrubMath.fineFactor : spec.step
        let raw = min(max(value + sign * amount, spec.range.lowerBound), spec.range.upperBound)
        return option ? min(max(SliderSpec.snap(raw, step: amount), spec.range.lowerBound), spec.range.upperBound)
            : spec.normalize(raw)
    }

    /// Scroll-wheel adjustment. Line-based wheels move one step per line; precise
    /// (trackpad) deltas are points and move `step` per 4 points. ⌥ = fine.
    public static func scroll(_ value: Double, deltaY: Double, precise: Bool, spec: SliderSpec, option: Bool = false) -> Double {
        guard deltaY != 0, deltaY.isFinite else { return value }
        let steps = precise ? deltaY / 4 : deltaY
        let step = option ? spec.step / ScrubMath.fineFactor : spec.step
        let raw = min(max(value + steps * step, spec.range.lowerBound), spec.range.upperBound)
        return min(max(SliderSpec.snap(raw, step: step), spec.range.lowerBound), spec.range.upperBound)
    }

    /// Track drag. Normal: the knob follows the pointer. Fine (⌥ held): the value
    /// moves by the drag distance at a tenth of the track's speed from where the drag began.
    public static func trackDrag(startValue: Double, startX: Double, x: Double, width: Double, spec: SliderSpec,
                                 fine: Bool) -> Double {
        let w = max(width, 1)
        guard fine else { return spec.value(atFraction: x / w) }
        let span = spec.range.upperBound - spec.range.lowerBound
        let raw = startValue + (x - startX) / w * span / ScrubMath.fineFactor
        let clamped = min(max(raw, spec.range.lowerBound), spec.range.upperBound)
        let step = spec.step / ScrubMath.fineFactor
        return min(max(SliderSpec.snap(clamped, step: step), spec.range.lowerBound), spec.range.upperBound)
    }
}

/// Ends an edit gesture after input goes idle (L3: one undo step per gesture).
/// Drags end on mouse-up; scroll-wheel and arrow nudges have no "up", so a
/// burst of nudges is one gesture that ends `delay` after the last one.
@MainActor
public final class GestureIdleEnder {
    private var task: Task<Void, Never>?
    private var pendingEnd: (() -> Void)?
    public let delay: Duration
    public var isActive: Bool { pendingEnd != nil }

    public init(delay: Duration = .milliseconds(600)) { self.delay = delay }

    /// Input happened; `end` runs once input has been idle for `delay`.
    public func touch(_ end: @escaping () -> Void) {
        pendingEnd = end
        task?.cancel()
        let delay = delay
        task = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled else { return }
            self?.fire()
        }
    }

    private func fire() {
        let end = pendingEnd
        pendingEnd = nil
        end?()
    }

    /// End now (e.g. focus left the control).
    public func flush() {
        task?.cancel()
        task = nil
        fire()
    }
}

/// Whether a tool control (slider, curve, wheel) currently has keyboard focus.
///
/// The culling key monitor must let arrow keys through while this is true, so
/// arrows nudge the focused control instead of moving to the next photo.
@MainActor @Observable
public final class ControlFocus {
    public static let shared = ControlFocus()
    /// Number of focused tool controls (focus can hand over between two views in one update).
    public private(set) var focusedCount = 0
    public var isToolControlFocused: Bool { focusedCount > 0 }

    /// Bumped by `resignAll()`: every focused tool control lets go of the keyboard.
    public private(set) var resignGeneration = 0

    public init() {}

    public func focusChanged(_ focused: Bool) {
        focusedCount = max(0, focusedCount + (focused ? 1 : -1))
    }

    /// [cul9] H6: a click in the viewer or the browser hands the arrow keys back to culling.
    public func resignAll() {
        guard focusedCount > 0 else { return }
        resignGeneration &+= 1
    }

    /// [cul9] H6: after a pointer gesture on a control, does it keep keyboard focus?
    /// A plain click does (to nudge with ←/→ or the scroll wheel, as the help says);
    /// a drag hands the keys straight back to culling (edit, then → to the next photo).
    public static func keepsFocus(afterPointerMovedBy distance: Double) -> Bool {
        distance < dragThreshold
    }

    /// Points a pointer must move before a press is a drag.
    public static let dragThreshold: Double = 3
}

/// Tool panel width: default, bounds, and drag math for the resize handle.
public enum PanelWidth {
    public static let `default`: Double = 320
    public static let range: ClosedRange<Double> = 260...480
    public static let storageKey = "toolPanelWidth"

    public static func clamp(_ w: Double) -> Double {
        guard w.isFinite else { return `default` }
        return min(max(w, range.lowerBound), range.upperBound)
    }

    /// Width after dragging the trailing edge by `dx` from `start`.
    public static func dragged(start: Double, dx: Double) -> Double { clamp(start + dx) }

    /// Stored width (0 / missing → default).
    public static func stored(_ defaults: UserDefaults) -> Double {
        let v = defaults.double(forKey: storageKey)
        return v == 0 ? `default` : clamp(v)
    }
}
