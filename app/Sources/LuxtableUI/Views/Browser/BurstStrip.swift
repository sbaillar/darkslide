import LuxtableCore
import SwiftUI

/// The burst strip: every frame of the collapsed burst whose badge was clicked, in a row
/// just above the filmstrip. It rises from the filmstrip with a spring, its frames fanning
/// out from the stack's cell (a plain fade with Reduce Motion), and folds back the same way.
/// A click shows a frame; "Use as Cover" (⏎, the hover button or the context menu) makes it
/// the stack's cover. Esc, a click outside or the badge again closes it. The keys stay with
/// the main keyboard router (`AppModel.handleBurstStripKey`), so rating and flag keys go
/// to the shown frame as always.
package struct BurstStrip: View {
    @Bindable var model: AppModel
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    package init(model: AppModel) { self.model = model }

    package var body: some View {
        let open = model.burstStripShown
        ZStack(alignment: .bottom) {
            if open {
                // A click anywhere else over the viewer closes the strip.
                Color.clear.contentShape(.rect)
                    .onTapGesture { model.closeBurstStrip() }
                    .accessibilityHidden(true)
                BurstStripContent(model: model, picker: model.browserState.picker, reduceMotion: reduceMotion)
                    .transition(reduceMotion ? .opacity : .burstFan)
            }
        }
        .animation(reduceMotion ? .easeInOut(duration: 0.18) : .spring(response: 0.42, dampingFraction: 0.8),
                   value: model.browserState.picker.stackID)
        .animation(reduceMotion ? .easeInOut(duration: 0.18) : .spring(response: 0.42, dampingFraction: 0.8), value: open)
    }
}

/// How far the strip has opened (0: folded into the stack, 1: open), for its frames.
private struct BurstFanKey: EnvironmentKey {
    static let defaultValue = 1.0
}

extension EnvironmentValues {
    fileprivate var burstFan: Double {
        get { self[BurstFanKey.self] }
        set { self[BurstFanKey.self] = newValue }
    }
}

/// The strip's open/close transition: it rises and fades in, and hands its progress to
/// the frames, which fan out from the stack's cell.
private struct BurstFanModifier: ViewModifier, Animatable {
    var progress: Double

    nonisolated var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func body(content: Content) -> some View {
        content
            .environment(\.burstFan, progress)
            .opacity(min(1, progress * 1.6))
            .offset(y: (1 - progress) * 36)
    }
}

extension AnyTransition {
    fileprivate static var burstFan: AnyTransition {
        .modifier(active: BurstFanModifier(progress: 0), identity: BurstFanModifier(progress: 1))
    }
}

private struct BurstStripContent: View {
    @Bindable var model: AppModel
    let picker: BurstPicker
    let reduceMotion: Bool

    private static let cell = CGSize(width: 110, height: 96)

    var body: some View {
        let frames = model.burstStripOrder // highest rating first
        let decision = self.decision
        let cover = decision?.cover
        VStack(alignment: .leading, spacing: DS.Space.xs) {
            HStack(spacing: DS.Space.s) {
                Image(systemName: "square.stack.3d.up.fill").foregroundStyle(DS.accent)
                Text("Burst of \(frames.count)").font(.system(size: 11, weight: .semibold))
                Text("Highest rated first. ← → to look through, ⏎ to use as cover, Esc to close")
                    .font(.system(size: 10)).foregroundStyle(DS.textSecondary)
                    .lineLimit(1)
                Spacer(minLength: DS.Space.m)
                Button("Use as Cover", systemImage: "checkmark.seal") { model.useAsCover() }
                    .disabled(picker.current.map { $0 == cover || !(decision?.candidates.contains($0) ?? false) } ?? true)
                    .help(picker.current.flatMap { model.coverHint(for: $0) } ?? "Show this frame when the burst is collapsed (⏎)")
                Button("Close", systemImage: "xmark") { model.closeBurstStrip() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .help("Close (Esc)")
            }
            .controlSize(.small)
            ScrollViewReader { proxy in
                ScrollView(.horizontal) {
                    LazyHStack(spacing: DS.Space.xs) {
                        ForEach(Array(frames.enumerated()), id: \.element) { i, id in
                            frame(id, index: i, of: frames.count, decision: decision)
                        }
                    }
                    .padding(.vertical, DS.Space.xxs)
                }
                .scrollIndicators(.never)
                .frame(height: Self.cell.height + 2 * DS.Space.xxs)
                .onAppear { if let c = picker.current { proxy.scrollTo(c, anchor: .center) } }
                .onChange(of: picker.current) { _, c in
                    guard let c else { return }
                    withAnimation(reduceMotion ? nil : .snappy(duration: 0.18)) { proxy.scrollTo(c, anchor: .center) }
                }
            }
        }
        .padding(DS.Space.s)
        .background(DS.chrome.opacity(0.96), in: .rect(cornerRadius: DS.Radius.m))
        .overlay(RoundedRectangle(cornerRadius: DS.Radius.m).strokeBorder(DS.hairline))
        .shadow(color: .black.opacity(0.35), radius: 10, y: 3)
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: CGFloat(frames.count) * (Self.cell.width + DS.Space.xs) + 2 * DS.Space.s + DS.Space.xs)
        .padding(.horizontal, DS.Space.m)
        .padding(.bottom, DS.Space.s)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Burst frames")
    }

    /// Why the collapsed stack shows the frame it shows, from current ratings.
    private var decision: CoverDecision? {
        picker.stackID.flatMap { model.coverDecision(of: $0) }
    }

    @ViewBuilder private func frame(_ id: PhotoID, index: Int, of count: Int, decision: CoverDecision?) -> some View {
        if let item = model.item(id) {
            let isCover = decision?.cover == id
            BurstFrame(item: item, size: Self.cell, isCurrent: picker.current == id, isCover: isCover,
                       coverLabel: isCover ? Self.label(decision) : "", canBeCover: decision?.candidates.contains(id) ?? false,
                       hint: model.coverHint(for: id),
                       index: index, count: count, anchorX: picker.anchor?.midX, confirmations: picker.confirmations,
                       justConfirmed: picker.confirmedCover == id, reduceMotion: reduceMotion, cache: model.thumbnails,
                       onShow: { model.showInBurstStrip(id) }, onUseAsCover: { model.useAsCover(id) })
                .id(id)
        }
    }

    /// "Best Shot" for the burst's Best Shot; "Your pick" or "AI pick" when they broke a tie
    /// for the top rating; else "Cover".
    static func label(_ d: CoverDecision?) -> String {
        guard let d else { return "Cover" }
        if d.source == .bestShot { return "Best Shot" }
        guard d.isTie else { return "Cover" }
        switch d.source {
        case .user: return "Your pick"
        case .ai: return "AI pick"
        case .rule, .bestShot: return "Cover"
        }
    }
}

/// One frame of the strip: its thumbnail (from the thumbnail cache), the cover mark, and
/// "Use as Cover" on hover.
private struct BurstFrame: View {
    let item: PhotoItem
    let size: CGSize
    let isCurrent: Bool
    let isCover: Bool
    /// "Cover", "Your pick", "AI pick" or "Best Shot".
    let coverLabel: String
    /// Tied for the stack's highest rating: Use as Cover is offered.
    let canBeCover: Bool
    /// What it takes to be the cover, when it can't be.
    let hint: String?
    let index: Int
    let count: Int
    /// The stack cell's centre (window coordinates): where the frame fans out from.
    let anchorX: CGFloat?
    let confirmations: Int
    let justConfirmed: Bool
    let reduceMotion: Bool
    let cache: ThumbnailCache
    let onShow: () -> Void
    let onUseAsCover: () -> Void
    @Environment(\.burstFan) private var fan
    @State private var hovering = false

    var body: some View {
        // Later frames follow a little behind, so the burst deals out from the stack.
        let lag = count > 1 ? 0.35 * Double(index) / Double(count - 1) : 0
        let p = min(1, max(0, (fan - lag) / (1 - lag)))
        let anchorX = anchorX
        ThumbnailCell(item: item, size: size, style: .filmstrip, isSelected: false, isFocused: isCurrent, cache: cache)
            .overlay(alignment: .topLeading) {
                if isCover {
                    Label(coverLabel, systemImage: coverLabel == "AI pick" ? "sparkles" : "checkmark.seal.fill")
                        .font(.system(size: 9, weight: .bold))
                        .padding(.horizontal, 5).padding(.vertical, 2)
                        .background(DS.accent, in: .capsule)
                        .foregroundStyle(.black)
                        .symbolEffect(.bounce, value: reduceMotion ? 0 : confirmations)
                        .padding(DS.Space.xs)
                        .transition(.scale.combined(with: .opacity))
                        .accessibilityHidden(true)
                }
            }
            .overlay(alignment: .topTrailing) {
                if item.photo.rating > 0 {
                    StarsView(rating: item.photo.rating, size: 8)
                        .padding(.horizontal, 4).padding(.vertical, 2)
                        .background(.black.opacity(0.6), in: .capsule)
                        .padding(DS.Space.xs)
                        .accessibilityHidden(true)
                }
            }
            .overlay(alignment: .bottom) {
                if hovering, !isCover {
                    if canBeCover {
                        Button("Use as Cover", action: onUseAsCover)
                            .font(.system(size: 10, weight: .semibold))
                            .buttonStyle(.borderedProminent)
                            .controlSize(.mini)
                            .padding(.bottom, 16)
                            .transition(.opacity)
                    } else if let hint {
                        Text(hint)
                            .font(.system(size: 9, weight: .medium))
                            .multilineTextAlignment(.center)
                            .padding(.horizontal, 5).padding(.vertical, 2)
                            .background(.black.opacity(0.7), in: .rect(cornerRadius: 3))
                            .foregroundStyle(.white)
                            .padding(.horizontal, DS.Space.xs)
                            .padding(.bottom, 16)
                            .transition(.opacity)
                    }
                }
            }
            .keyframeAnimator(initialValue: 1.0, trigger: confirmations) { content, scale in
                content.scaleEffect(justConfirmed && !reduceMotion ? scale : 1)
            } keyframes: { _ in
                SpringKeyframe(1.08, duration: 0.14)
                SpringKeyframe(1.0, duration: 0.3)
            }
            .visualEffect { content, proxy in
                let dx = anchorX.map { $0 - proxy.frame(in: .global).midX } ?? 0
                return content
                    .offset(x: dx * (1 - p))
                    .scaleEffect(0.55 + 0.45 * p, anchor: .bottom)
            }
            .opacity(p)
            .onHover { h in withAnimation(.easeOut(duration: 0.12)) { hovering = h } }
            .onTapGesture(perform: onShow)
            .contextMenu {
                if canBeCover {
                    Button("Use as Cover", systemImage: "checkmark.seal", action: onUseAsCover).disabled(isCover)
                } else if let hint {
                    Text(hint)
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("\(item.photo.filename), \(item.photo.rating) stars, \(index + 1) of \(count)\(isCover ? ", \(coverLabel)" : "")")
            .accessibilityAddTraits(isCurrent ? [.isButton, .isSelected] : .isButton)
            .accessibilityAction { onShow() }
            .accessibilityAction(named: canBeCover ? "Use as Cover" : (hint ?? "Use as Cover"), onUseAsCover)
    }
}
