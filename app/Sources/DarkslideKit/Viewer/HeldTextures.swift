import Metal

/// What each pane last presented, so a frame whose texture isn't uploaded yet never
/// leaves the pane blank: the previous texture stays up for that pass (drag flicker:
/// a slider drag delivers a new frame every few ms; skipping the pane until the
/// upload landed presented an empty view in between, at slider rate).
struct HeldTextures {
    struct Entry {
        let id: UInt64
        let texture: MTLTexture
        let region: SIMD4<Float>
        let aspect: Double
    }

    private(set) var slots: [Int: Entry] = [:]

    /// The texture to draw in pane `slot` for `frame`: `ready` when uploaded (it
    /// becomes the held one), else the held texture when it shows the same region at
    /// the same aspect (the same photo, an older edit), else nil. `held` names the frame
    /// the returned texture belongs to when it isn't `frame` (keep it alive).
    mutating func resolve(slot: Int, frame: RenderedFrame, region: SIMD4<Float>,
                          ready: MTLTexture?) -> (texture: MTLTexture?, held: UInt64?) {
        let aspect = Double(frame.width) / Double(max(frame.height, 1))
        if let ready {
            slots[slot] = Entry(id: frame.id, texture: ready, region: region, aspect: aspect)
            return (ready, nil)
        }
        guard let e = slots[slot], e.region == region, abs(e.aspect / aspect - 1) < 0.01 else {
            // Not drawn this pass: its texture may be recycled, so it can't be held later.
            slots[slot] = nil
            return (nil, nil)
        }
        return (e.texture, e.id)
    }

    /// Forget slots past `count` (the plan has fewer panes now).
    mutating func trim(to count: Int) {
        slots = slots.filter { $0.key < count }
    }
}
